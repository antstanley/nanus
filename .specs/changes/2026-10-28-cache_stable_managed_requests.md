# Change: Cache-stable managed requests

**Status:** Implemented — 2026-10-28, with the deviations below. Every deterministic acceptance
case has a passing test. Of the live cases, only the subscription plan's ran: its acceptance passed
and its cache read failed (see *Deviations as built*); C16 and the API-plan and DeepSeek cases of C17
were not run. Accepted 2026-10-28 by the owner, after an independent review (findings folded in),
and again after the owner put the legacy elision notice and the Responses system carrier in scope ·
**Date:** 2026-10-28 · **Owner:** Ant Stanley · **Target:** `nanus-domain` (managed compiler,
notice, working-data message, layout version, legacy fitter's notice), `nanus-ports` (`ChatRequest`
tail and cache retention, `ManagedSupport` layout version), `nanus-adapter-anthropic` (cache
breakpoints, canonical encoding, tail block), `nanus-adapter-openai` (tail carrier, Responses system
carrier, explicit breakpoints), `nanus-adapter-deepseek` (tail), `nanus-bundle` (request composition,
legacy tail, retention, readiness, admission view), `nanus-link` and `nanus-tui` (status cache
share), `docs/context-management.md`, `docs/design.md`, `docs/features.md`, `docs/roadmap.md`,
`docs/tui.md`, `AGENTS.md`

Managed requests will be laid out so that provider prompt caches can serve them. Today the
fixed-schema notice is a system message directly after the system prompt, and it changes on every
step. Every provider caches by prefix, so on every wire only the tool schemas and the system prompt
can be cache hits, and the whole conversation is billed uncached on every step.

- **The tail.** The notice will move to a tail that each adapter places after the newest message,
  in the form that wire's cache can work around. On Anthropic, the tail is the last block of the
  final user turn, behind an explicit cache breakpoint. On OpenAI, it is a trailing `developer`
  message, which the provider's implicit breakpoint never lands on. On the other chat vendors, it
  is appended to the final message.
- **The working-data message** will keep only what changes when the notes or the goal objective
  change. The hidden-history catalog moves to the tail.
- **Anthropic breakpoints.** The Anthropic adapter, which today asks for no caching at all, will
  send `cache_control` breakpoints when the runner opts in. The stock runner does, in both modes.
- **Cache share on the status.** The link's context status will report the cache share of the last
  model request, in both modes, so the effect can be seen and not only argued.
- **Signed replay.** Anthropic signed thinking replay keeps today's conservative rule by default. A
  per-model switch, promoted only on live evidence, lets replay survive from one step to the next.
- **The legacy elision notice.** It is a system message today, so every dropped turn rewrites
  Anthropic's whole system field. It moves to the same tail.
- **The Responses system prompt.** It moves from `instructions` into a leading `developer` message,
  where an explicit breakpoint can mark it. Different sessions with the same tools and prompt can
  then share its cache entry.

---

## Motivation

Provider prompt caches match a request against cached prefixes. A managed request starts with the
tool schemas, then the system prompt, then the notice (`prepare.rs:278`).

- **The notice changes every step.** It carries `estimate_input_tokens` and `pressure_percent`, and
  both change on every step because the history grows. It also carries the revision, the counts,
  the goal phase and revision, and the budget hint (`compile.rs:401`).
- **Every wire puts it ahead of the conversation.** On Anthropic it is joined into the top-level
  `system` field (`crates/nanus-adapter-anthropic/src/wire.rs:75`). On Responses it goes into
  `instructions` (`responses.rs:48`). On chat completions it is a second system message.

In each case the first byte that differs from the previous step comes before the first user
message. So every step pays full input price for the whole conversation, and a long session is
billed at many times what a cache-aware layout would cost. `docs/context-management.md` already
records the symptom ("costs the earlier reasoning and the prompt cache").

Two other things make it worse:

- **Anthropic gets no cache hits in either mode.** The adapter never sends `cache_control`, and the
  Messages API caches only at breakpoints. The usage decoder (`wire.rs:696`) reads cache counters
  that are always zero.
- **The working-data message changes too often.** It sits directly after the first user message and
  carries the goal revision, phase and provenance, the context revision, a staleness marker and the
  hidden-id catalog. A goal phase change, or any hide anywhere in the history, invalidates almost
  the whole conversation.

A layout that merely moves the notice is not enough, because each provider decides where it writes
cache entries:

- **OpenAI.** On GPT-5.6 and later, which is every model nanus offers
  (`crates/nanus-adapter-openai/src/config.rs:61-68`), the implicit breakpoint is placed "at the end
  of the latest eligible message". Eligible messages are user messages and the last tool response
  of a group. A tail appended inside that message is therefore inside every cache write, and no
  later request can read the write.
- **DeepSeek.** It writes a unit at the end of the user input, so any tail costs it about one step
  of reuse.

The design below places the tail differently per wire for that reason.

The legacy path has the same flaw on a slower clock:

- **The elision notice.** When the whole-turn fitter drops turns, it inserts a system message after
  the leading prompt, carrying the dropped counts (`notice_for`,
  `crates/nanus-domain/src/context.rs:367`). On Anthropic that message is joined into `system`, so
  each drop rewrites the cache from the tool schemas on, not just from the first kept turn.
- **`instructions`.** On Responses, the system prompt travels in `instructions`, which cannot carry
  an explicit breakpoint. The only entry OpenAI is sure to write is at the end of the newest eligible
  message. Two sessions with identical tools and prompt therefore have no entry they are guaranteed
  to share: a new `nanus run` can start cold even though its first several thousand tokens match the
  last run's.

---

## Baseline this builds on

- **Composition:** `AgentRunner::compose` (`prepare.rs:257-283`) builds every candidate's messages
  as `[system(managed_system_prompt), notice, seed?, effective.messages...]`.
  - `managed_system_prompt` (`prepare.rs:115`) is the runner's system prompt followed by
    `MANUAL_POLICY_V1`.
  - `effective_request` (`prepare.rs:616`) uses the same compose for admission and commit.
  - The admission view's raw request clones `fitted_base` and swaps in the raw messages
    (`crates/nanus-bundle/src/agent_loop/admission.rs:50-56`).
- **The compiler:** `derive_effective_context` (`crates/nanus-domain/src/context/managed/compile.rs`)
  returns `Effective { notice, seed, messages, memory, hidden, protected }`.
  - `render_memory` (`:253`) writes `MEMORY_LABEL` (`:59-61`, which names "the hidden-history
    catalog"), then:
    - the goal line with revision, phase and provenance (`:266`);
    - the notes header with the context revision, the goal binding and `STALE` (`:278`);
    - the notes;
    - `catalog` (bounded by `limits::CATALOG_BYTES_MAX`, 2 KiB).
  - `render_notice` (`:401`) writes the `key=value` line and three fixed sentences.
  - The message is inserted after the first user message only when something follows it
    (`:207-216`).
- **Versions:** `limits::POLICY_VERSION = 1`.
  - A recorded `ContextPolicy` with any other version is refused (`records.rs:171`, `:442`), so
    bumping the policy version would make every existing managed session unreadable.
  - `ManagedSupport::Supported { policy_version }` (`crates/nanus-ports/src/llm.rs:155-165`) is the
    only thing an adapter declares.
- **Fitting:** `hard_fit` (`fit.rs`) hides the oldest eligible fragments and, once it has to act,
  continues to the 60 % target. That hysteresis already keeps fits rare. This change does not alter
  it.
- **Anthropic encoding:** `wire::build_counted` is shared by the ordinary and managed paths.
  - `system_text` joins every system message into one string (`wire.rs:114`).
  - Tool results are gathered into one `user` turn (`:156-231`).
  - An assistant turn's signed blocks are replayed only when `replay.prefix_digest ==
    prefix_fingerprint(system, tools, turns)` (`:213`).
  - The digest recorded for a reply is `request_prefix(payload)` over the sent body (`lib.rs:230`,
    `managed.rs:217`).
  - No `cache_control` is sent anywhere in the repository.
- **OpenAI and DeepSeek encoding:** chat messages are encoded one by one (`openai/wire.rs:185`,
  `deepseek/wire.rs:157`). Responses joins system messages into `instructions` and encodes flat
  items (`responses.rs:48`, `:91`, `:122`). No cache field is sent.
  - The opt-in stateless Responses replay mode captures the leading system messages as a sealed
    instruction revision (`responses/request/instructions.rs:16-48`). Its receipts bind them as
    `instructions`.
  - `tests/ordinary_responses_bytes.rs` pins the ordinary Responses body on both plans with
    `"instructions":"Be brief."`.
- **The legacy fitter:** `fit_with_source` (`crates/nanus-domain/src/context.rs:279-335`) keeps the
  leading prompt, drops whole turns oldest first, and inserts `notice_for`'s system message
  (`:367-377`) between the leading prompt and the first kept turn.
  - Its cost callback is `FnMut(&[Message]) -> u32`, so the notice is costed as a message.
  - `build_request_for` (`crates/nanus-bundle/src/agent_loop.rs:956`) is the one assembly for legacy
    turns and maintenance requests. It runs the fitter only when the request carries images or the
    runner has a request reservation.
  - `a_trimmed_prompt_carries_a_notice_and_a_report` (`context.rs:618`) pins the notice at index 1.
- **Usage:**
  - Every wire decodes `cache_hit_tokens` into the neutral `Usage`. An absent counter decodes as
    zero, so a reported zero and an unreported counter are indistinguishable.
  - Each assistant message event carries its `usage`.
  - Each managed `request/attempt` record carries a `UsageObservation` (`records.rs:718-735`).
  - The domain `ContextStatus` (`tooling.rs:286-322`) is `deny_unknown_fields` and is also
    `ManageResult.context` (`tooling.rs:357-362`), which the model reads.
  - The link's `ContextStatusInfo` (`crates/nanus-link/src/protocol.rs:385`) is a separate type,
    built by `wire_status` (`crates/nanus-link/src/server/context.rs:187`).
  - `context_status_key` (`crates/nanus-tui/src/runtime.rs:4627`) reads fields by name.
- **Tests that pin today's layout:**
  - the notice text in the three adapters' `managed_tests.rs`, `crates/nanus-link/tests/link.rs`
    and `compile/tests.rs`;
  - the working-data message's index in `crates/nanus-bundle/tests/managed_context.rs:353-359`;
  - the seed at index 2 in `crates/nanus-bundle/tests/maintenance.rs:476`;
  - the sent body as the fingerprint input in `crates/nanus-adapter-anthropic/src/managed_tests.rs:470-474`;
  - each adapter's ordinary body.

---

## Affected spec pages

The canonical description of nanus is `docs/` plus `AGENTS.md` (see `.specs/README.md`). The
boundary schema (`docs/context-management.schema.json`) does not change. The domain `ContextStatus`
and `ManageResult` keep their shape, so nothing new becomes model-visible.

| Canonical page | Nature of change |
|---|---|
| [`docs/context-management.md`](../../docs/context-management.md) → introduction | Restate the legacy guarantee: the model reads the same content; only cache hints are added |
| `docs/context-management.md` → What a managed request carries | Rewrite: the tail replaces the notice message, the working data is narrowed, three-part layout and its invariant |
| `docs/context-management.md` → Providers | Replace the signed-replay paragraph; grammar sentence; ordinary-bytes sentence in the matrix intro |
| `docs/context-management.md` → Providers → Request layout and provider caches | Add |
| `docs/context-management.md` → Providers → The Responses wire | Modify the intro and the `instructions` bullet; add the tail and the head breakpoint |
| `docs/context-management.md` → Goals and the manual policy | Add `MANAGED_REQUEST_GUIDE_V1` |
| `docs/context-management.md` → Handoff → The seed | Drop "and the notice" from the seed's position |
| `docs/context-management.md` → Human controls and clients | The status carries the last request's cache share |
| `docs/context-management.md` → What is and is not verified | Rewrite the legacy bullet; add verified and not-verified items |
| `docs/context-management.md` → What a managed request carries → reset-to-legacy sentence | The legacy notice is in the tail |
| [`docs/design.md`](../../docs/design.md) | Add *Requests are laid out for provider caches* |
| [`docs/features.md`](../../docs/features.md) → `context_budget` bullet and Legacy context | The notice is at the end of the request, not at the gap |
| [`docs/roadmap.md`](../../docs/roadmap.md) → item 21 | Same correction to "the model reads a notice at the gap" |
| [`docs/tui.md`](../../docs/tui.md) → `/context status` | The `context:` line shows the cache share |
| [`AGENTS.md`](../../AGENTS.md) → Invariants | Modify "Managed context is opt-in…"; add "A request's prefix is stable" |

---

## Proposed changes

### `docs/context-management.md` → introduction (Modify)

Replace the second paragraph's first sentence with:

> A session that never enables it reads, runs and saves exactly as before, with the same tool count
> and the same event trace. Its model is sent the same content as before, with two differences in
> where things sit, both shared by every legacy session and neither caused by managed context:
>
> - the fitter's elision notice, when turns were dropped, is the request's tail;
> - on Responses, the system prompt is a leading developer message.
>
> Its requests also carry cache hints (see
> [Request layout and provider caches](#request-layout-and-provider-caches)).

### `docs/context-management.md` → What a managed request carries (Modify, whole section)

> Compilation (`derive_effective_context`) is fixed in order:
>
> 1. Fold the raw replay, derive the fragments, and check the accepted revision against its base.
> 2. Keep every original user message and every protected fragment; hidden ids remove whole
>    eligible fragments and nothing else; what survives keeps its original order.
> 3. Render the accepted notes and the goal objective as one labelled **assistant** text message,
>    with no calls, reasoning or replay, immediately after the earliest user message. Its text
>    changes only when the note array or the goal objective changes. Revision numbers, the goal's
>    phase, revision and provenance, note staleness and the hidden-history catalog are stated in the
>    tail, never here. A session with neither notes nor a goal has no such message, even when
>    fragments are hidden.
> 4. Never make that message the last one: a fresh request with nothing after its first user
>    message carries no generated data, and the tail says `goal_data_available=false`.
> 5. Render the **tail**: the fixed-schema notice line and, when anything is hidden, the bounded
>    recovery catalog. The notice line states the counts, revision, estimate and its estimator,
>    allowance, pressure, the goal's phase, revision and provenance, note staleness, and recovery
>    availability. Only numbers, enums and opaque ids appear in it, never anything retrieved or
>    written by a model. The tail is not a message. It travels in `ChatRequest::context_tail`, and
>    the adapter places it after the newest message (see
>    [Request layout and provider caches](#request-layout-and-provider-caches)). It is never
>    recorded in the session log, so recall never finds it and no fragment contains it.
>
> A managed request has three parts, in this order:
>
> | Part | Holds | Changes when |
> |---|---|---|
> | Head | Tool schemas; the system prompt with `MANUAL_POLICY_V1` and `MANAGED_REQUEST_GUIDE_V1`; a seed; the first user message; the working-data message | The model, effort, schemas, policy, seed, note array or goal objective changes |
> | Body | The selected conversation, in event order | It grows by appending. It changes in place only when a revision hides or restores a fragment, and then only from the earliest fragment the revision changed |
> | Tail | The notice line and the catalog | Every request |
>
> **Nothing that changes from one step to the next comes before the newest message.** Take two
> consecutive requests of a session with no revision, goal, policy, schema, model or effort change
> between them. Remove the tail and the cache markers from both. The earlier request's encoding is
> then a prefix of the later one's.
>
> There is one exception. When notes or a goal exist before a session's first prompt (a goal set
> with `/goal`, or a seeded session's notes), the first request has no working-data message,
> because it would be last. The second request inserts it after the first user message. Every
> adapter is tested for the rule and for this exception.
>
> Deterministic hard fitting acts only when the candidate is over the hard allowance, hiding the
> oldest eligible complete fragment first. Every probe is compiled and prepared by the selected
> adapter, tail included, so its cost is the cost of the body that adapter would send. A protected
> floor above 60 % but within the allowance succeeds; above the allowance it refuses with
> `protected_floor_too_large`, and no partial hidden set is installed. A successful fit is an
> automatic revision persisted with the request intent before dispatch. The catalog is in the tail,
> so a fit that changes no notes leaves the head unchanged. After a reset to legacy, the legacy
> whole-turn fitter applies again. Its notice is that request's tail, placed the same way.

### `docs/context-management.md` → Providers (Modify)

Replace "Each supported path splits its dispatch into one encode and a separate send, so the ordinary
path's bytes are unchanged and the prepared call sends exactly what it estimated and digested" with:

> Each supported path splits its dispatch into one encode and a separate send. The ordinary path
> sends what an ordinary request of the same build sends, and the prepared call sends exactly what
> it estimated and digested.

Update the grammar sentence to read "leading system messages only (the system prompt and a seeded
session's handoff seed)", and add: "and a conversation that ends with a user message or tool
results, which is where the tail goes".

Replace the Anthropic signed-replay paragraph ("Anthropic signed replay is sent only when…") with:

> Anthropic signed replay is sent only when the adapter's existing check accepts it: the original
> blocks under the encoded prefix that produced them. The prefix fingerprint never includes cache
> markers.
>
> - **By default it includes the tail.** A step's tail is gone from the next request, so a managed
>   turn's thinking is not replayed after the step that produced it. That is no loss against what
>   managed sessions got before. The same holds for a legacy reply produced under an elision notice:
>   it is sent neutrally on later requests, where before this change it was replayed until the
>   fitter's next drop. A legacy reply produced with no elision has no tail and replays exactly as
>   before.
> - **For a model whose live acceptance is recorded** (`replay_across_tail` in the adapter's model
>   table), the fingerprint excludes the tail, and signed blocks survive from one step to the next.
>
> When hiding changes a turn's prefix, that turn is sent in its neutral form — its text and tool
> uses, without the thinking blocks — which the Messages API accepts as an edited history. A turn
> sent neutrally is sent the same way on every later step, so the cache is unaffected either way.
> Signatures and digests are never rewritten. A turn that carries only signed replay and has no
> neutral form refuses preparation.

### `docs/context-management.md` → Providers → Request layout and provider caches (Add, after the Provider matrix)

> Every adapter encodes a request in two steps:
>
> 1. **Canonical encoding** is the body the wire would carry with no tail and no cache hints.
> 2. **Decoration** adds the tail and the wire's cache hints. It is a pure function of the
>    canonical body, `context_tail` and `prompt_cache`.
>
> A message encodes to the same bytes whether or not it is the newest one, apart from the tail's own
> bytes. The request digest (`PreparedModelCall::request_digest`), the estimate and the dispatch all
> read the decorated body, which is what is sent. The Anthropic signed-replay prefix digest reads the
> body without cache markers, and without the tail when the model allows it.
>
> | Wire | Where the system prompt goes | Where the tail goes | Cache hints |
> |---|---|---|---|
> | Messages (Anthropic) | The top-level `system` field: one text block per system message, in order | Its own `text` block, the last block of the final `user` turn, after any `tool_result` blocks | Up to four `cache_control` breakpoints |
> | Responses at OpenAI | A leading `developer` message: one `input_text` part per system message, in order | A trailing `developer` message, after the final user item or tool results | Up to two explicit `prompt_cache_breakpoint`s on the API plan: the end of the system prompt and the end of the head |
> | Chat completions at OpenAI | Leading `system` messages, as today | A trailing `developer` message, after the final user message or tool results | None |
> | Chat completions at DeepSeek, z.ai and Inception | Leading `system` messages, as today | Appended to the final message's content: a trailing `text` part when the content is a list, otherwise after a blank line in the content string | None |
>
> Both modes have a tail:
>
> - **Managed:** the notice line and the catalog, on every request.
> - **Legacy:** the whole-turn fitter's elision notice, only on a request that dropped turns.
>
> Every request a runner builds ends with a user message or tool results. Every adapter's managed
> grammar check refuses a request that does not, and the legacy builder refuses to attach a tail to
> one. So there is always a place for the tail. The tail is identified by position:
>
> - **Anthropic:** the last block of the request.
> - **OpenAI:** the trailing developer message.
> - **Other chat vendors:** the text after the final message's own content.
>
> A `[nanus context]` line anywhere else, inside a tool result for example, is that message's
> content.
>
> **Retention.** `ChatRequest::prompt_cache` is `None` unless the runner opts in, so a library host
> that builds a `ChatRequest` itself pays no cache-write premium it did not ask for. The stock runner
> opts in:
>
> - `Short` for `nanus run`;
> - `Long` for an agent served over the link, where a person may pause between turns.
>
> Under `Short` every breakpoint is five minutes. Under `Long` every breakpoint is one hour. A cache
> write costs 1.25 times the input price for five minutes and twice the input price for one hour. A
> cache read costs a tenth of the input price, so a five-minute entry pays for itself on its second
> read and a one-hour entry on its third.
>
> **Anthropic breakpoints** are computed over the encoded `messages` array:
>
> 1. the first `system` block, which is the harness's system prompt. A seed or a compaction summary
>    follows in later blocks, so the entry is shared by every session with the same model, tools and
>    prompt;
> 2. the last block of the head: the working-data message when present, otherwise the first `user`
>    message;
> 3. the last block of the message immediately before the last `assistant` message. That is where
>    the previous request wrote its entry, so the next request reads it even when more than twenty
>    blocks were appended in between;
> 4. the last block before the tail, or the request's last block when there is no tail.
>
> A breakpoint is never placed on a `thinking` block; the search walks back past them. Candidates
> are taken in position order, and a candidate at or before one already placed is dropped. So
> there are at most four, and their TTLs never increase from start to end. Breakpoint 2 exists only
> in managed requests. A prefix shorter than the model's minimum cacheable length is simply not
> cached.
>
> **OpenAI.** On GPT-5.6 and later, OpenAI places an implicit breakpoint at the end of the latest
> eligible message: a user message, or the last tool response of a group. A trailing developer
> message is not eligible, so the implicit breakpoint lands before the tail. The next request finds
> that entry among the earlier eligible message endings it looks up.
>
> On Responses, the system prompt is a leading developer message rather than top-level
> `instructions`. `instructions` cannot carry a breakpoint, and a developer message part can. On the
> API plan, when the runner opted into caching, two parts carry
> `prompt_cache_breakpoint: {"mode":"explicit"}`:
>
> 1. the leading developer message's first part, which is the harness's system prompt, in both
>    modes. A seed or a compaction summary follows it in later parts, so the breakpoint falls before
>    anything session-specific. A session's requests write and read this entry, so a seed, a
>    summary or a change to the head after it still reads the prompt from cache;
> 2. in managed requests only, the head's last `input_text` part: the working-data message when
>    present, otherwise the first user item. A fit that leaves the head unchanged still reads it.
>
> These are OpenAI's first two explicit breakpoints, which it always looks up. The implicit
> breakpoint stays on and covers the growing conversation.
>
> The opt-in stateless replay mode keeps `instructions`. Its sealed receipts bind the instructions
> in that form, and it is not a managed path.
>
> The session's id is sent as `prompt_cache_key` whenever the runner opted into caching, on
> both plans and on both wires at OpenAI's own endpoints: a plan changes who pays, not how a
> session's requests reach its cache. On models before GPT-5.6 a stable key is how related requests
> are routed to the same cache; on later ones routing is automatic and the key separates cache
> groups. So one session's requests are kept together as its conversation grows, and a session does
> not read another's entries, the system prompt's included. A gateway configured as OpenAI, z.ai and
> Inception are sent no key, and the opt-in stateless replay mode sends none.
>
> **DeepSeek** writes a cache unit at the end of the request's input and another at the end of its
> output, and it matches only whole units. Both of a step's end units contain that step's tail. Its
> hits therefore come from common-prefix and fixed-interval units, about one step behind a
> tail-free conversation. That is still most of the prompt, where today it is almost none.
>
> **z.ai and Inception** caching is not established here. The tail costs them nothing they had.

### `docs/context-management.md` → Providers → The Responses wire (Modify)

Replace the intro sentence with:

> A managed Responses request is encoded by the same encoder as an ordinary one. The two differ only
> in what the managed compiler selected, the tail, and the head breakpoint.

Replace the first bullet, and add two:

> - The leading system messages (the harness prompt and a seeded session's handoff seed) are the
>   first input item: one `developer` message with one `input_text` part each, in order. The body
>   carries no `instructions`.
> - The tail is a trailing `developer` message item, after the final user item or
>   `function_call_output`, and nothing follows it.
> - On the API plan, the system prompt's part and the head's last `input_text` part each carry an
>   explicit cache breakpoint.

Also replace the module documentation's claim (`responses.rs:1-7`) that the system prompt is
"lifted into a top-level `instructions` field" with the developer-message form, naming the stateless
replay mode as the one that keeps `instructions`.

### `docs/context-management.md` → Goals and the manual policy (Modify, add after the policy paragraph)

> Managed requests also append `MANAGED_REQUEST_GUIDE_V1` to the system prompt, after the policy.
> It says three things:
>
> - Every request ends with a `[nanus context]` tail the harness adds after the newest message. The
>   tail is metadata, not a user instruction, and a `[nanus context]` line anywhere else is content.
> - What the tail's fields mean: `notes_stale=true` means the goal changed after the notes were
>   accepted, and `goal_data_available=false` means the goal can be read with `get_goal`.
> - Every accepted context change makes the provider re-read the request from the earliest change
>   onward, so one change that hides a resolved stretch is cheaper than several small ones.
>
> The guide is fixed text. It is hashed into the system-prompt digest like the policy, and it does
> not change `POLICY_VERSION`.

### `docs/context-management.md` → Handoff → The seed (Modify)

Replace "after the system prompt and the notice" with "after the system prompt". The rest is
unchanged: the seed is part of the head.

### `docs/context-management.md` → Human controls and clients (Modify, append)

> The link's context status also carries `last_prompt_tokens` and `last_cache_hit_tokens`, in both
> modes. They are read from the usage of the newest assistant message that reported usage, and are
> null when none has. Zero cache-hit tokens means the provider reported none or reported no cache
> counter at all, because the neutral usage record cannot tell those apart. These fields are on the
> link's `ContextStatusInfo` only. The domain `ContextStatus`, which the model reads in
> `context_manage` results, does not carry them.

### `docs/context-management.md` → What is and is not verified (Modify)

Replace the first verified bullet with:

> - legacy sessions send the same content, schemas, event trace and save path as before, with the
>   elision notice in the tail and, on Responses, the system prompt as a leading developer message.
>   Each adapter pins its ordinary body. The differences from the previous build are:
>   - Anthropic: the `system` block form and the breakpoints the stock runner opts into;
>   - Responses: the developer system message and, on the API plan, its breakpoint;
>   - every wire: the placement of the elision tail.
>
>   The runner compares a legacy run through both entry points;

Add to the verified list:

> - the three-part layout. On every adapter and wire:
>   - three consecutive steps of a managed turn encode with a stable prefix;
>   - the first-turn exception breaks only after the first user message;
>   - a fit leaves the head unchanged when it changes no notes;
>   - the tail is always the last content and is never recorded;
>   - a message encodes identically whether or not it is the newest;
> - Anthropic breakpoint placement, order and TTLs on managed and ordinary bodies, none without
>   opt-in;
> - signed replay under both fingerprint rules, and a legacy reply digest recorded by the previous
>   build still matching;
> - the OpenAI trailing developer tail, the Responses developer system message, both explicit
>   breakpoints, and the session's `prompt_cache_key` on both plans at OpenAI's endpoints and on no
>   other path;
> - the legacy fitter: its elision notice is costed and carried as the tail, never as a message, on
>   every wire, and the fitter's choice of turns is the same as before for the same budget.

Add to the not-verified list:

> - **Cache hit rates are not measured by the gates.** The `#[ignore]`d live cases in
>   `nanus-bundle/tests/live_cache.rs` check them:
>   - that each provider serves a managed step's prefix from cache at the stated thresholds;
   - that OpenAI accepts a trailing developer message after tool results, and a request with a
>     developer system message and no `instructions`, on both plans and wires;
>   - per Anthropic model, whether signed replay is accepted with an earlier step's tail removed.
>
>   They need credentials, and a pass is evidence for that day and model only.

### `docs/design.md` → Requests are laid out for provider caches (Add)

> A provider's prompt cache is most of what a long agent session costs, and it matches by prefix.
> So a request is ordered by how often its parts change:
>
> 1. the parts that never change: schemas, the system prompt and the seed;
> 2. the parts that change on a deliberate decision: the first prompt and the working data;
> 3. the conversation, which only grows;
> 4. last, the one part that changes every request: the context notice, or in a legacy session the
>    elision notice.
>
> Both notices used to be system messages, where they carried the harness's voice. They moved to
> the end because that voice cost the cache: the managed notice cost the whole conversation's on
> every step, and the legacy notice cost even the system prompt's on every drop.
>
> The system prompt is marked as a cache entry of its own, ahead of anything session-specific, so
> sessions that share a model, tools and prompt share it. On Anthropic it is the first `system`
> block. On Responses it is the first part of a leading developer message, because `instructions`
> cannot be marked.
>
> At the end it takes whatever form lets that wire's cache work around it. On Anthropic it is its own
> block behind an explicit breakpoint. On OpenAI it is a developer message, which the implicit
> breakpoint skips, so OpenAI keeps the harness's voice. Elsewhere it is appended text.
>
> It holds only numbers, enums and opaque ids, so placing it in a user turn promotes no model-written
> text. It is identified by its position, so a copy inside a tool result is read as content.
>
> Model-written notes stay in an assistant message near the start, and are kept stable instead of
> being moved. Moving them to the end would put them in the user's voice.

### `docs/tui.md` → `/context status` (Modify)

> The `context:` line ends with `cache N%` when the newest assistant message reported usage: the
> share of its prompt the provider served from cache. When no usage has been reported, the line
> leaves it out. `0%` is drawn as reported, and includes a provider that reports no cache counter.
> The share does not make a step's status a new line: like the estimates, it moves with every step.

### `docs/features.md` → `context_budget` bullet (Modify)

Replace "with a notice the model reads where the gap is" with "with a notice the model reads at the
end of the request".

### `docs/features.md` → Legacy context (Modify)

Replace "drops the oldest whole turns when the budget is spent, with a notice where the gap is" with:

> drops the oldest whole turns when the budget is spent. The model is told at the end of the
> request, after the newest message: the notice says how many messages and turns were left out and
> that the oldest thing shown is not the start of the conversation. The interface still draws the gap
> where it is.

### `docs/roadmap.md` → item 21 (Modify)

Replace "(the model reads a notice at the gap, and the reader is told through the CLI and the
interface)" with:

> (the model reads a notice at the end of the request, where it costs no cached prefix, and the
> reader is told at the gap through the CLI and the interface)

### `AGENTS.md` → "Managed context is opt-in, and a legacy session must not notice it exists" (Modify)

Replace "A session that never records a `context/mode` record keeps its version-2 body, request
bytes, schemas, tool count and terminal save" with:

> A session that never records a `context/mode` record keeps its version-2 body, the content its
> model is sent, schemas, tool count and terminal save. Its request bytes differ from the previous
> build in three ways, none of them caused by managed context:
>
> - the cache hints every runner-built request carries;
> - the elision notice, which is the request's tail;
> - on Responses, the system prompt, which is a leading developer message.

### `AGENTS.md` → Invariants (Add, after "Managed context is opt-in…")

> - **A request's prefix is stable.** Nothing that changes from one step to the next comes before
>   the newest message.
>   - The managed notice and catalog, and the legacy elision notice, are the request's tail
>     (`ChatRequest::context_tail`), which the adapter places last and never records.
>   - The system prompt is its own cache entry, ahead of a seed or compaction summary: the first
>     Anthropic `system` block, or the first part of the Responses developer system message, never
>     `instructions`. The stateless replay mode is the exception.
>   - The working-data message changes only with the notes or the goal objective. The one exception
>     is its first appearance in a session that has notes or a goal before its first prompt.
>   - Adapters encode canonically and then decorate. Cache markers never enter a signed-replay
>     fingerprint, and the tail enters it unless the model's live evidence says otherwise.
>   - OpenAI's tail is a trailing developer message, because an appended tail would be inside every
>     implicit cache write.
>   - Each adapter has a prefix-stability test. A change that puts a per-step value ahead of the
>     conversation fails it.

---

## Type changes

The boundary schema does not change. The Rust changes are these.

- **`nanus_ports::ChatRequest`** gains two fields:
  - `context_tail: Option<String>`. A managed request sets it from the compiler. A legacy request
    sets it to the elision notice when the fitter dropped turns. The admission view's raw request
    clears it.
  - `prompt_cache: PromptCacheRetention`.

  The struct is not `#[non_exhaustive]`, so the new public fields break struct-literal construction
  outside the workspace. The release notes say so.
- **`nanus_ports::PromptCacheRetention { None, Short, Long }`**, defaulting to `None`.
- **`nanus_ports::ManagedSupport::Supported`** gains `layout_version: u32`. That is a compile error
  for an out-of-tree adapter, deliberately, so that it cannot claim support while ignoring
  `context_tail`. The new constant `nanus_domain::context::managed::limits::LAYOUT_VERSION` is `2`.
- **`nanus_domain::context::managed::compile::Effective`** loses `notice: Message` and gains
  `tail: String`.
- **`nanus_domain::Session::last_reported_usage() -> Option<&Usage>`**: the usage of the newest
  assistant message that carries one.
- **`nanus_link::protocol::ContextStatusInfo`** gains `last_prompt_tokens: Option<u64>` and
  `last_cache_hit_tokens: Option<u64>`. Both are `#[serde(default)]`, and both are null exactly
  when the other is. `PROTOCOL_VERSION` does not move: an older peer reads the frame unchanged.
- **`nanus_domain::context`, the legacy fitter:**
  - `fit_with_source` and `fit_with` take a cost callback of
    `FnMut(&[Message], Option<&str>) -> u32`, where the second argument is the candidate's tail.
  - `Fitted.messages` no longer contains the notice.
  - `Elision` gains `notice(&self) -> String`, the text `notice_for` builds today, unchanged in
    wording.
  - `fit` costs the tail as `estimate` costs a system message of that text.
- **The OpenAI vendor table** gains these per-path fields:
  - `system_carrier` (`Developer` or `Instructions`);
  - `tail_carrier`;
  - `system_breakpoint`;
  - `head_breakpoint`.

  The Anthropic model table gains `replay_across_tail`.

---

## Design

### D1. The tail

`render_tail(session, selection, goal, memory, facts, protected, fragments)` replaces
`render_notice`.

- **First line:** today's `key=value` line, with one addition, `goal_set_by=`. Its value is the
  `GoalProvenance`, or `none` when there is no goal.
- **Second line:** present only when something is hidden. It is today's `catalog` output,
  unchanged.
- **The three fixed sentences** leave the notice and become part of `MANAGED_REQUEST_GUIDE_V1`.

A postcondition asserts that the tail is at most `limits::TAIL_BYTES_MAX = CATALOG_BYTES_MAX +
1024` bytes. The probe facts (`probe_facts`, `prepare.rs:82`) keep `estimate_input_tokens:
Some(u32::MAX)`, so a probe's tail is never shorter than the tail the frozen request carries.

### D2. The working-data message

`render_memory` keeps:

- the label;
- the goal line, reduced to `Goal: <objective>`;
- a notes header reduced to `Notes (bound to goal revision G):`;
- the notes.

It drops the context revision, the goal's revision, phase and provenance, the `STALE` suffix and the
catalog. It returns `None` when there are no notes and no goal.

**This changes what the model reads.** A session with hidden fragments but no notes and no goal
loses its working-data message, and learns what is hidden from the catalog in the tail.
`MEMORY_LABEL` drops "and the hidden-history catalog" and names only "accepted context notes and the
session goal". The adapters identify the message by the label's opening words, which are unchanged.
The module documentation (`compile.rs:1-20`) is rewritten to the new order.

**Cache breaks this design accepts.** The message first appears at the second step after notes or a
goal come into existence:

- a goal created during a turn breaks the cache once;
- a goal or notes present before the first prompt break it once, between the first two steps.

### D3. Composition and readiness

- **`compose`** (`prepare.rs:257`) builds `[system(managed_system_prompt), seed?,
  effective.messages...]`. It sets `request.context_tail = Some(effective.tail)` and
  `request.prompt_cache` from the runner.
- **`managed_system_prompt`** becomes
  `{system_prompt}\n\n{MANUAL_POLICY_V1}\n\n{MANAGED_REQUEST_GUIDE_V1}`.
- **`effective_request`** inherits this. Admission ports see the tail as a field, not as a
  message, and cost it through `LlmPort::estimate_request` like the rest of the body.
- **The raw request** in the admission view (`admission.rs:50-56`) sets `context_tail = None`, so
  the original view is costed as what a legacy request of the same messages would be.
- **Readiness.** `unready` and `check_managed_ready` (`prepare.rs:141`, `:158`) require
  `ManagedSupport::Supported { layout_version: LAYOUT_VERSION, .. }`, and answer `unsupported_mode`
  otherwise.

### D4. Retention from the runner

`AgentRunner` gains `with_prompt_cache(PromptCacheRetention)`, and the library default is `None`.

- Stock composition (`compose.rs`, `build_runner`) sets `Short`.
- `nanus-link`'s `serve` sets `Long` on the runner it serves with.
- `build_request` (`agent_loop.rs:941`), `base_request` (`prepare.rs:240`) and the maintenance
  builders (`agent_loop/maintenance.rs`, `managed/handoff.rs`) copy the runner's retention into
  every request they build.

### D5. Anthropic

`wire::build_counted` is split in two.

1. **`encode_canonical(config, request) -> (Value, skipped)`** is today's function. The replay
   check in `encode_messages_with_prefix` compares against fingerprints of canonical prefixes:
   - with the tail, by default;
   - without it, when `replay_across_tail` is set for the model.

   `request_prefix` fingerprints the same form. A canonical body has today's shape, so a reply
   digest recorded by an earlier build in a **legacy** session still matches. A managed session's
   earlier digests were taken over a body whose `system` carried the notice, so those never matched
   from one step to the next anyway.
2. **`decorate(canonical, request) -> Value`** does three things:
   - rewrites `system` as one `{"type":"text","text":…}` block per system message, in order. The
     canonical form keeps today's joined string, so fingerprints are unaffected;
   - appends the tail block to the final `user` turn;
   - places the breakpoints listed under *Request layout*, when `prompt_cache` is not `None`.

`build_counted` returns the decorated body, and the replay prefix digest beside it, for
`StreamAccumulator::with_prefix`.

`validate_sequence` (`managed.rs:263`) refuses, with `protocol_incompatible`, a managed request
whose conversation ends on anything but a user message or a tool result. The legacy builder refuses
the same case for a request it attaches a tail to (D10). `decorate` asserts that a request carrying
a tail ends on a `user` turn. There is no fallback, because both builders have already excluded the
case.

`replay_across_tail` starts `false` for every model, including `claude-fable-5-1`. A model is
promoted only by recording C16's pass for it.

### D6. OpenAI

`wire::build_request` and `responses::build_request` gain the same split.

- **The system carrier, on Responses.** On a path whose `system_carrier` is `Developer` (every
  Responses path, as shipped), the canonical encoding puts the leading system messages into one
  first input item: `{"role":"developer","content":[{"type":"input_text","text":…}, …]}`, one part
  per system message, in order. No `instructions` is sent.
  - `instructions(&request.messages)` (`responses.rs:91`) is used only on an `Instructions` path and
    by the stateless replay mode. That mode keeps `instructions` because
    `responses/request/instructions.rs` seals its receipts over that form.
  - Chat completions keep their leading `system` messages.
- **The system breakpoint.** On a path whose `system_breakpoint` is true (the Responses API plan,
  as shipped), decoration adds `"prompt_cache_breakpoint":{"mode":"explicit"}` to the developer
  system message's first part. That part is the harness's system prompt. It is placed in both
  modes, whenever `prompt_cache` is not `None`.
- **The tail carrier.** On a path whose `tail_carrier` is `TrailingDeveloper` (every path, as
  shipped), decoration adds a final developer message:
  - on chat, `{"role":"developer","content":tail}`;
  - on Responses, `{"role":"developer","content":[{"type":"input_text","text":tail}]}`.
- **The head breakpoint.** On a path whose `head_breakpoint` is true (the Responses API plan, as
  shipped), decoration adds `"prompt_cache_breakpoint":{"mode":"explicit"}` to the last
  `input_text` part of the head:
  - the working-data message, when present;
  - otherwise the first user item.

  It is placed in managed requests only, and only when `prompt_cache` is not `None`. No
  `prompt_cache_options` is sent, so the implicit breakpoint stays on.
- **C17 is a gate.** If it shows a path refusing a trailing developer message, that path's
  `tail_carrier` becomes `AppendedExplicit`. That carrier:
  - sends `prompt_cache_options: {"mode":"explicit"}`;
  - encodes every `function_call_output` of a managed body with an `input_text` array output;
  - places explicit breakpoints at the head, at the message before the last assistant message, and
    at the last part before the tail, as Anthropic's breakpoints 2–4 are placed;
  - appends the tail as a trailing `input_text` part.

  The deviation is recorded in this file.
- **Other C17 outcomes.**
  - If a path refuses the head or system breakpoint, that flag becomes false for the path.
  - If a path refuses a body with a developer system message and no `instructions` (the
    subscription endpoint is the one most likely to), that path's `system_carrier` becomes
    `Instructions`, and its `system_breakpoint` becomes false, since `instructions` cannot carry one.

### D7. DeepSeek, z.ai, Inception

Decoration appends the tail and sends no hints. An assistant message's `reasoning_content`, and
anything else in an earlier message, is unchanged.

### D8. Ordinary requests

The ordinary path calls the same encoders with the legacy tail (D10) and the runner's retention.

- **Anthropic.** With stock retention, ordinary bodies gain the per-message `system` blocks and
  breakpoints 1, 3 and 4. A first request, which has no assistant message, has 1 and 4.
- **Responses.** Ordinary bodies gain the developer system message in place of `instructions`. On
  the API plan with stock retention, they also gain its breakpoint.
- **Chat completions at every vendor.** Ordinary bodies are unchanged, except for an elided
  request's tail.

Each pinned ordinary body is updated in the same commit (`tests/ordinary_responses_bytes.rs`'s two
goldens and the Anthropic pins), and the diff is only what this list names. The runner's legacy
comparison through both entry points still holds, because both produce the new bytes.

### D10. The legacy elision notice

`fit_with_source` (`crates/nanus-domain/src/context.rs:279`) no longer inserts the notice. Each
candidate is the leading prompt and the kept turns. The cost callback is passed the candidate and
`elision.map(Elision::notice)` as its tail. The notice is still costed, by its tail placement rather
than as a message. The fitter's choice of turns is therefore the same for a given budget, except
where the few framing bytes the placement changes decide a boundary case. The fitter's tests pin
the ordinary cases and one boundary case each way.

`notice_for`'s wording is kept. It already says the oldest thing shown is not the start, which
reads correctly at the end of the request, and its doc comment is rewritten to say where it now
sits and why.

`build_request_for` (`crates/nanus-bundle/src/agent_loop.rs:956`) does three things:

- it sets `request.context_tail` to the elision's notice, and its probe requests do the same, so
  the probe costs exactly what is sent;
- it refuses with `BundleError::context` a fitted request with a tail whose last message is neither
  a user message nor a tool result. No current builder produces one, and maintenance requests end
  with their fixed instruction as a user message;
- it never panics.

The elision report (`Elision`, the `elided` frame, the CLI's stderr line and the TUI's gap entry) is
unchanged. The reader is still told at the gap, and only the model's copy moves.

A legacy request that dropped no turns has no tail, so its bytes are D8's alone.

Responses has no signed-replay fingerprint outside the stateless mode, which keeps `instructions`.
So moving its system prompt into the canonical encoding invalidates no digest a session recorded.
The request digests in `request/attempt` records name what was sent and are never compared against
a later encoding.

### D9. Observing the cache

The link computes the cache share; the domain does not.

- `wire_status` (`crates/nanus-link/src/server/context.rs:187`) takes `session.last_reported_usage()`
  beside the domain status, and fills `last_prompt_tokens` and `last_cache_hit_tokens` from its
  `prompt_tokens` and `cache_hit_tokens`.
- It does this for every status frame the server builds, legacy included: an idle status, a
  requested one, and a step status.
- The domain `ContextStatus` and `ManageResult` are unchanged, so the model never sees the share.

The TUI changes in two places:

- **`context_status_line`** (`crates/nanus-tui/src/runtime.rs:4642`) destructures the two fields.
  When both are present and the prompt count is non-zero, it appends `cache N%`: the floor of hit ÷
  prompt × 100, using checked arithmetic.
- **`context_status_key`** (`runtime.rs:4627`) does not read them. It reads fields by name, so the
  compiler will not catch a mistake here. C14 does.

---

## Implementation notes

Order matters: domain, then ports, then adapters, then runner, then clients. That way each step
compiles and its tests can be written against the layer below.

```
1. nanus-domain
   - compile.rs: render_tail (from render_notice + catalog + goal_set_by); MANAGED_REQUEST_GUIDE_V1;
     narrowed render_memory; MEMORY_LABEL text; module docs; Effective { tail }.
   - limits.rs: TAIL_BYTES_MAX, LAYOUT_VERSION. session.rs: last_reported_usage.
   - compile/tests.rs: memory stable across goal phase change and hide-only revisions; tail
     contents and bound; no memory with hidden-only.
   - context.rs: fit_with_source / fit_with / fit with the tail-aware cost; Elision::notice;
     notice_for's doc comment; a_trimmed_prompt_carries_a_notice_and_a_report and the notice-cost
     tests rewritten to find the notice in the tail, not at index 1.
2. nanus-ports/src/llm.rs: ChatRequest::{context_tail, prompt_cache}; PromptCacheRetention;
   ManagedSupport::Supported { layout_version }.
3. nanus-adapter-anthropic: wire.rs (encode_canonical / decorate, per-message system blocks,
   breakpoints, tail block, fingerprint rule), config.rs (replay_across_tail per model), lib.rs,
   managed.rs
   (validate_sequence ending rule; support's layout_version; module doc: replace "In practice the
   fixed-schema notice changes…" with the fingerprint rule and the grammar's new leading messages).
4. nanus-adapter-openai: wire.rs (split, developer tail), responses.rs (split, developer system
   message, developer tail, system and head breakpoints, module doc lines 1-7), config.rs
   (system_carrier, tail_carrier, system_breakpoint, head_breakpoint), managed.rs (ending rule,
   layout_version, module doc line 33 "(the harness prompt, then the fixed-schema notice)").
   The stateless replay path (responses/request.rs, request/instructions.rs) keeps instructions;
   stateless_tests.rs must still pass unchanged.
5. nanus-adapter-deepseek: wire.rs (appended tail), managed.rs (ending rule, layout_version, module
   doc line 16).
6. nanus-bundle: prepare.rs (compose, managed_system_prompt, readiness), agent_loop.rs
   (build_request, build_request_for with the legacy tail and its ending check, probe tails,
   with_prompt_cache), admission.rs (clear the raw tail), maintenance.rs, managed/handoff.rs,
   compose.rs (Short).
7. nanus-link: server (Long; wire_status with usage at every status frame), protocol.rs
   (ContextStatusInfo fields; round-trip with and without them).
8. nanus-tui/src/runtime.rs: context_status_line, context_status_key, their tests.
9. Tests that pin the old layout, updated:
   - notice text: the three adapters' managed_tests.rs, nanus-link/tests/link.rs, compile/tests.rs;
   - nanus-bundle/tests/managed_context.rs:353-359 (no working-data message after a hide-only
     proposal with no notes and no goal; indexes shift by one);
   - nanus-bundle/tests/maintenance.rs:476 (the seed is at index 1, after the system prompt);
   - nanus-adapter-anthropic/src/managed_tests.rs:470-474 (fingerprint the replay prefix form,
     not prepared.body());
   - every adapter's ordinary-body pin: Anthropic and Responses change as D8 says, the chat pins
     must not;
   - Responses tests that read body["instructions"]: responses.rs:801 and :816,
     managed_tests.rs:883-897 and :1205-1235, compaction_wire_tests.rs:122-134,
     tests/live_wire.rs:358, tests/ordinary_responses_bytes.rs (both goldens);
   - legacy elision tests in nanus-bundle that look for the notice among the messages.
10. nanus-bundle/tests/live_cache.rs (new, every case #[ignore]d): C16, C17.
11. Docs and AGENTS.md per Proposed changes.
```

References:

- Anthropic Messages API: `cache_control` (`ephemeral`, `ttl` `5m` or `1h`); at most four
  breakpoints; longer TTLs before shorter; a twenty-block lookback; not on `thinking` blocks; text
  after `tool_result` blocks; write prices 1.25× and 2×.
- OpenAI prompt caching for GPT-5.6 and later: implicit breakpoint eligibility; lookup boundaries;
  `prompt_cache_breakpoint`; `instructions` cannot carry a breakpoint, a developer message's
  `input_text` part can.
- DeepSeek context caching: end-of-input and end-of-output units, matched whole.

### Acceptance cases

Each case is a deterministic test unless it is marked *live*.

| # | Case |
|---|---|
| C1 | The compiled request has no system message after the system prompt and seed. `context_tail` holds a line with every key, `goal_set_by` included, and a catalog line exactly when something is hidden. The tail is within `TAIL_BYTES_MAX` at the hidden-id cap. |
| C2 | The working-data text is byte-identical before and after a goal phase change, a goal revision with the same objective, a hide-only revision, and a fit. It changes on a new note array or a new objective. A session with hidden fragments and no notes and no goal has no working-data message. `MEMORY_LABEL` no longer mentions the catalog. |
| C3 | *Prefix stability*, per adapter and wire: Anthropic, OpenAI chat, Responses on the API and subscription plans, DeepSeek, z.ai, Inception. Three consecutive steps of a managed turn, the third crossing into a new human turn. With the tail and markers removed, each body's `system`, `tools` and message or input prefix (the Responses developer system message included) equal the previous one's. A third fixture runs a legacy session past its budget: between two drops, consecutive requests are prefix-stable in the same sense. A second fixture sets a goal before the first prompt. Its first two steps differ only from the end of the first user message on, and its later steps are stable. |
| C4 | *A fit breaks only the body*: a step that fits with no notes keeps the head (through the first user message, and the working-data message when present) identical to the step before. |
| C5 | *The tail is last*: <ul><li>Anthropic: its own text block after every `tool_result` block of the final user turn.</li><li>OpenAI chat and Responses: a trailing developer message after tool results and after a user message.</li><li>DeepSeek, z.ai, Inception: appended to a final `tool` string, a final `user` string, and a final `user` part list.</li></ul> The same message, not last, encodes without it. A managed request ending on an assistant message is refused `protocol_incompatible` by every adapter. |
| C6 | *Anthropic breakpoints* on managed bodies: positions 1–4 as specified, at most four, none on a thinking block, and none at or before one already placed. TTLs never increase in position order. `Long` makes every breakpoint one hour, and `Short` makes every one five minutes. Fixtures include step 2 of a first turn with a working-data message (`[U1, Mem, A1, Tool]`), where breakpoint 3 would coincide with breakpoint 2 and is dropped. |
| C7 | *Anthropic breakpoints* on ordinary bodies: 1, 3 and 4 only; a first request has 1 and 4. Breakpoint 1 is on the first `system` block even when a seed or compaction summary follows it. `PromptCacheRetention::None` emits no marker, but still the per-message `system` blocks. |
| C8 | *Signed replay*: <ul><li>With `replay_across_tail` false, a managed turn's signed blocks are sent neutrally on the next step, and identically on every step after.</li><li>With it true, they are replayed on the next step and on the first step of the next turn.</li><li>Under either setting, they are sent neutrally after a hide that changes their prefix.</li><li>A legacy-session reply digest recorded by the previous build still replays.</li><li>A legacy reply produced under an elision tail is sent neutrally on the next request when `replay_across_tail` is false, and replayed when it is true. A legacy reply produced with no tail replays as before.</li></ul> |
| C9 | *Digest and estimate*: the decorated body is what `estimate.request_bytes`, the request digest and the frozen dispatch all see. The local listener receives exactly those bytes. |
| C10 | *OpenAI hints*: <ul><li>The system breakpoint is on the developer system message's first part, in both modes.</li><li>The head breakpoint is on the last `input_text` part of the working-data message, or of the first user item when there is none, in managed requests only.</li><li>Both are on the Responses API plan only, and neither appears under `PromptCacheRetention::None`.</li><li>No body anywhere carries `prompt_cache_options`, except under `AppendedExplicit`, which has its own fixtures for its three breakpoints and array outputs.</li><li>With caching asked for, the session's `prompt_cache_key` is on both plans' Responses bodies and on chat at OpenAI's endpoint, the same on both plans, and on no gateway, z.ai or Inception body; never under `PromptCacheRetention::None`.</li></ul> |
| C11 | *The tail is not recorded*: after a managed turn, no event in the log and no `context_recall search` hit contains `[nanus context]` unless a tool result produced it. |
| C12 | *A forged notice*: a tool result containing a `[nanus context]` line compiles and encodes with the real tail after it, on each carrier. Grammar validation is unaffected. |
| C13 | *The guide and versions*: <ul><li>The managed system prompt ends with `MANAGED_REQUEST_GUIDE_V1`.</li><li>`MANUAL_POLICY_V1` and `POLICY_VERSION` are unchanged.</li><li>A proposal staged under the old system-prompt digest is stale.</li><li>An adapter declaring `layout_version: 1` makes the session unready with `unsupported_mode`.</li></ul> |
| C14 | *Status*: <ul><li>`last_prompt_tokens` and `last_cache_hit_tokens` are null before any usage, then set from the newest assistant message with usage, in a managed and in a legacy session.</li><li>The domain `ContextStatus` and `ManageResult` serialise exactly as before.</li><li>The link round-trips a status with and without the fields.</li><li>The TUI line shows `cache 75%` for 300 of 400, `cache 0%` for 0 of 400, and nothing when null.</li><li>Two step statuses differing only in these fields draw one line.</li></ul> |
| C15 | *Legacy*: <ul><li>A legacy request that dropped no turns has no `context_tail`. One that dropped turns has the elision notice as its tail, and no notice among its messages.</li><li>The runner's two-entry-point comparison matches.</li><li>The admission view's raw request carries no tail.</li><li>`build_request_for` refuses a tailed request whose last message is an assistant message, without panicking.</li><li>The `elided` frame, the CLI line and the TUI's gap entry are unchanged.</li></ul> |
| C16 | *live*, per Anthropic model (`claude-opus-5-5`, `claude-sonnet-5-5`, `claude-fable-5-1`): <ul><li>Step 2 of a managed turn reports `cache_read_input_tokens` of at least the previous request's input, less its tail.</li><li>A request replaying the previous step's signed blocks with that step's tail removed is accepted. A pass sets `replay_across_tail` for that model; a failure leaves it false and is recorded here.</li></ul> |
| C17 | *live*, OpenAI, on the API and subscription plans, chat (API plan) and Responses: <ul><li>A trailing developer message after `function_call_output` or tool messages is accepted.</li><li>A Responses body with a developer system message and no `instructions` is accepted, and answers as the system prompt directs.</li><li>The system and head breakpoints are accepted where they are sent.</li><li>Step N+1 of a managed turn reports cached tokens of at least step N's input, less its tail, rounded down to the provider's cache granularity.</li></ul> *live*, DeepSeek: step N+2 reports `prompt_cache_hit_tokens` of at least step N's input. A miss on any path applies D6's fallback and is recorded here. |
| C18 | *The legacy fitter*: <ul><li>For the existing fitter fixtures, the kept turns are the same as before for the same budget.</li><li>One boundary fixture each way shows the tail's framing deciding a drop.</li><li>Every probe is costed with its tail.</li><li>A compaction or handoff draft request that the fitter trims carries the notice as its tail, after its fixed instruction.</li></ul> |
| C19 | *The Responses system carrier*: <ul><li>An ordinary and a managed Responses body, on both plans, carry no `instructions`. Their first input item is a developer message with one part per system message, in order: the system prompt, then a seed or a compaction summary.</li><li>An `Instructions` path, and the stateless replay mode, still send `instructions`, and `stateless_tests.rs` passes unchanged.</li><li>The two ordinary goldens are replaced with the developer form.</li></ul> |

---

## Benchmark

The compiler and the encoders are on the managed hot path. Run `managed_turn` and `wire` per
[`docs/benchmarks.md`](../../docs/benchmarks.md), with the parent revision as `before`. The tail
moves bytes but adds none beyond `goal_set_by`. Decoration walks the final turn and inserts at most
four markers. Any regression above noise is reported in this file.

### Results (2026-10-28)

`managed_turn` and `wire` were run on the parent revision (`--save-baseline before`, from a separate
jj workspace sharing the target directory) and on the change (`--baseline before`), on the same
machine. The allocation and byte counters are deterministic and are the evidence; wall time on this
machine that day was not, and is not used.

- **A regression found and fixed.** The first comparison showed `wire/encode/*/anthropic` allocating
  85–99% more: every plain encode computed the signed-replay prefix digest, which copied and
  serialised the whole message array, and `request_prefix` copied it again. `build_request` now
  computes no digest (only a dispatch needs one), and the fingerprint serialises borrowed fields in
  the same key order, byte for byte the earlier digest (`the_borrowed_fingerprint_is_the_earlier_one_byte_for_byte`).
  The first comparison also showed Responses encodes allocating ~3% more, from a tool output's text
  being copied twice; it is now moved.
- **After the fix, against the parent:**
  - `wire/encode` allocations: DeepSeek and OpenAI chat unchanged; Anthropic +0.32% at 10 turns and
    +0.05% at 100 (one `system` block per message); Responses −0.6% to −2.6%, bytes −9% to −18%
    (one developer part in place of the `instructions` string, and no double copy of tool output).
  - `wire/stream` allocations: −0.01% to −0.1%.
  - `managed_turn` allocations: −0.01% to −0.14% on every case; bytes +0.02% to +0.40% on the managed
    overhead cases (the guide in the system prompt) and −0.02% to −0.25% on the fitted ones.
- **Wall time is not reported as a regression.** In the same runs, paths whose code and allocation
  counts are identical to the parent's (DeepSeek encode, OpenAI chat encode without a tail) moved by
  +4% to +213%, so the machine's timing noise exceeds any effect this change could have; the
  time verdicts Criterion printed are not evidence either way.

---

## Merge plan

1. Apply each *Proposed changes* block to its page and bump `docs/context-management.md`'s date.
2. Add the *Requests are laid out for provider caches* section to `docs/design.md`, apply the
   `docs/features.md`, `docs/roadmap.md` and `docs/tui.md` blocks, and apply both `AGENTS.md`
   blocks.
3. Record C16's per-model result and C17's per-path result in a *Deviations as built* section of
   this file. If any path fell back, also record it in the *Request layout* block before applying
   that block.
4. If the test count changed, note it per `AGENTS.md`. The quoted numbers are not chased unless
   asked.
5. Flip this file's status to `Merged`, add `**Merged:** YYYY-MM-DD`, move it to
   `.specs/changes/merged/`, and update `.specs/README.md`.

---

## Assumptions and open questions

### Assumptions

- **Anthropic** caches at block boundaries where an earlier request wrote an entry, with a
  twenty-block lookback per breakpoint. It accepts a `system` array, `cache_control` on `text`,
  `image`, `tool_use` and `tool_result` blocks, and text after `tool_result` blocks in a user turn.
  These were checked against the provider's documentation on 2026-10-28.
- **OpenAI**, on GPT-5.6 and later, places the implicit breakpoint at the end of the latest eligible
  message. A developer message outside the initial group is not eligible. Lookups include up to
  twenty earlier eligible message endings and the first two explicit breakpoints. This was checked
  against the provider's prompt-caching guide on 2026-10-28. Some points are not documented, and
  C17 establishes them:
  - whether a trailing developer message is *accepted* after tool output;
  - whether the subscription endpoint accepts a body with no `instructions`.
- **A leading developer message carries the same authority as `instructions`.** For the offered
  reasoning models, the provider treats both as developer-level instructions. C17's
  "answers as the system prompt directs" check is what this rests on.
- **DeepSeek** caching is automatic and unit-based, as described above. **z.ai and Inception**
  caching was not checked. The tail does not depend on it.
- **Anthropic signed replay.** Whether Anthropic validates the context before a signed thinking
  block is unresolved. The cookbook says a signature "validates the conversation context", and the
  main guide only forbids modifying the blocks themselves. The default fingerprint therefore keeps
  today's behaviour, and C16 decides per model.

### Decisions

- *Where the notice goes.* **At the end, in each wire's cheapest form.** Every system position is
  ahead of the conversation on every wire. The notice is numbers, enums and opaque ids, so a user
  turn's lower authority costs nothing, and position identifies it.
- *OpenAI's carrier.* **A trailing developer message.** Appending to the final message would put the
  tail inside OpenAI's implicit cache write on every request, so no write would ever be read. A
  developer message after the conversation is not eligible for the implicit breakpoint. It also
  keeps the harness's voice. The explicit-only fallback is specified in case a path refuses it.
- *Other chat vendors' carrier.* **Appended to the final message.** A new trailing user message
  after tool results would read as the user interrupting the tool loop, and would open a new turn
  in DeepSeek's thinking mode. A mid-conversation system or developer message is not established
  for DeepSeek, z.ai or Inception.
- *`prompt_cache_key`.* **The session's id, on both plans** (owner's decision, 2026-10-28,
  superseding "dropped"). A subscription request is an API call like any other, so both plans send
  the same key at OpenAI's endpoints. Per session, because a session's requests are the ones that
  share a growing prefix; on models before GPT-5.6 a stable key is what routes them to one cache,
  and on the subscription the session's later steps missed without one. The cost is that sessions
  and `nanus run` invocations no longer share the system-prompt entry: separate keys are separate
  cache groups. No other provider uses one.
- *The working-data message stays near the start.* **Stabilised in place, not moved.** It holds
  model-written notes, which must not appear in a user turn, and an assistant message cannot be
  last. Moving it later would also move it every turn.
- *Catalog in the tail.* **Yes.** In the working-data message, any hide anywhere invalidated the
  whole conversation. In the tail, a hide invalidates only from the hidden fragment. The cost is up
  to 2 KiB re-sent uncached each step.
- *No policy version bump.* **A separate `MANAGED_REQUEST_GUIDE_V1` and a new layout version.**
  `POLICY_VERSION` is checked for equality on every recorded policy, so changing it would make
  existing sessions unreadable. The layout version lives on `ManagedSupport`, which is not
  recorded, so it costs no session anything. It forces an out-of-tree adapter to declare that it
  places the tail.
- *Anthropic breakpoint 3.* **The message before the last assistant message.** That is where the
  previous request wrote, so a step that appends a wide parallel batch still hits. The
  twenty-block lookback alone does not guarantee that.
- *Explicit Anthropic breakpoints, not top-level `cache_control`.* **Explicit.** The top-level marker
  goes on the last cacheable block, which would be the tail, so the next request could never read
  its entry.
- *Retention.* **Off by default; `Short` for `nanus run`; `Long` for link-served agents; one TTL per
  request.**
  - `Long` costs about 60 % more per step's write than `Short`. It pays back whenever a person pauses
    more than five minutes between turns, which is common at the interface and rare in `nanus run`.
  - One TTL per request keeps the ordering rule trivially true.
  - Off by default means a library host never pays a premium it did not choose.
- *Ordinary requests.* **The same layout rules, through the stock runner.** Legacy Anthropic
  sessions get no cache hits today either, and markers do not change what the model reads. Ordinary
  Responses requests gain the developer system message and its breakpoint. Ordinary chat requests
  change only by an elided request's tail.
- *The legacy elision notice.* **In the tail, by the owner's decision on 2026-10-28.**
  - As a system message it sat ahead of everything on Anthropic, so each drop cost the system
    prompt's cache as well as the conversation's.
  - In the tail, a drop costs only from the first kept turn, which changed anyway.
  - The model still reads the same sentence. The reader is still shown the gap where it is.
  - The cost is the reasoning continuity of replies produced under an elided request, until a
    model's `replay_across_tail` is promoted.
- *The Responses system prompt.* **A leading developer message, by the owner's decision on
  2026-10-28.** `instructions` cannot carry a breakpoint, so no request could mark the shared
  tools-and-prompt prefix for other sessions to read. One part per system message keeps the
  harness's prompt ahead of anything session-specific, so the breakpoint is on a prefix every
  session shares. The stateless replay mode keeps `instructions`, because its sealed receipts bind
  that form and it is not a managed path.
- *Anthropic `system` as one block per message.* **Yes.** For the same reason as on Responses:
  breakpoint 1 marks the harness's prompt alone, so a seed or a compaction summary does not make
  the entry session-specific. The canonical form keeps the joined string, so no recorded fingerprint
  changes.
- *Signed replay across steps.* **Off until each model's live evidence.** If Anthropic validates the
  preceding context, a relaxed fingerprint would turn every managed step after the first into a 400.
  The conservative rule costs only reasoning continuity, which managed sessions do not have today.
- *Cache share is link-only.* **Yes.** The domain status is what the model reads in
  `context_manage` results. A per-step number there would be noise to the model, and would change
  a `deny_unknown_fields` type an older build reads.
- *Fitting unchanged.* **Yes.** The existing 60 % hysteresis already makes fits rare. The guide
  states the cost of a model proposal rather than enforcing it.

### Open questions

(None at this stage. The live gates, C16 and C17, decide per-model and per-path settings by
evidence, and their fallbacks are specified above.)

---

## Deviations as built

- **Stateless Responses replay carries the elision notice in `instructions`.** D10 puts the legacy
  notice in the tail "on every wire", and C19 requires `stateless_tests.rs` to pass unchanged; those
  tests pin the notice inside `instructions`. The stateless mode keeps `instructions` because its
  sealed receipts bind them, and it is about sealed replay rather than a provider cache, so its tail
  is appended to `instructions` after the prompt: exactly the bytes it sent when the notice was a
  system message. Its projection check reads the tail (below).
- **The projection check takes the tail.** `identify_projection` and
  `identify_tool_result_projection` take the candidate's tail. A candidate is the leading prompt and
  the kept turns; its tail must be exactly the notice its omission implies, and absent when nothing
  was omitted. A notice among the messages proves no elision.
- **Idle dry preparations measure the next request.** The ending rule (a managed request ends with a
  user message or tool results) also refuses the dry preparation of an idle session, which ends with
  the model's answer: the status read and a staged proposal's revalidation. Both now measure the
  request the next prompt would start, with an empty prompt standing in for the one not yet typed.
  Nothing prepared that way is sent. Real steps are unchanged.
- **Every raw view clears the tail.** Besides the admission view (D3), the step-record projection's
  raw request on both the legacy and the managed path clears `context_tail`, for the same reason.
- **A tail with no place is an error at the port, not a panic.** `ChatRequest::tail_is_placeable`
  and `ChatRequest::check_tail` are new. Every adapter's `stream_chat` and `estimate_request` call
  `check_tail` first, so a library host that attaches a tail to a request ending on an answer is
  refused with `protocol_incompatible`. The encoders still assert, as D5 says, for a caller that
  skips the port.
- **Readiness codes.** An adapter that declares support with another `layout_version` makes the
  session unready with `unsupported_mode` (C13). An adapter that declares no support keeps today's
  `protocol_incompatible`.
- **The Responses head breakpoint is on the first user item.** The spec's reference documents a
  breakpoint on an `input_text` part; the working-data message is an `assistant` item whose part is
  `output_text`. So the head's breakpoint is on the head's last `input_text` part, the first user
  item's, and the working data after it is re-read each step. (The first build marked the working
  data's `output_text`; the second review flagged that placement as undocumented.)
- **The OpenAI chat tail is a trailing developer message only at OpenAI's endpoint.** A gateway or
  local server configured as `OpenAI` has no established mid-conversation developer role, so it
  takes the appended carrier, like z.ai and Inception (`a_chat_gateway_takes_the_tail_appended`).
- **The carrier table.** `OpenAiConfig::cache_path(protocol)` returns a `CachePath` with the four
  per-path fields. `SystemCarrier` has a third value, `System`, for chat completions. The breakpoint
  flags are true only at the official API base URL. A z.ai or Inception path on Responses, which no
  stock route produces, would use `Instructions` and a trailing developer tail.
- **`AppendedExplicit` is a Responses carrier.** Under it every tool output is an `input_text` list
  in both modes, not only in a managed body, so a legacy elision tail also has a part to join. No
  shipped path uses it.
- **`RENDERER_VERSION` does not move.** It is bound into every recorded revision's notes digest,
  which is checked on read, so moving it would make every managed session unreadable. Its doc
  comment now says so; `LAYOUT_VERSION` versions the layout.
- **`Frame::ContextStatus`'s payload is boxed.** The two new fields pushed the frame enum's largest
  variant over clippy's size threshold. A `Box` is transparent on the wire, so `PROTOCOL_VERSION`
  does not move.
- **A served agent's retention is set on the shared runner.** The link holds the runner behind an
  `Rc`, so `AgentRunner` also has `set_prompt_cache`, which `serve` calls with `Long`.
- **Two or more system messages are fingerprinted as the block list.** D5 kept the joined `system`
  string in the canonical form so no recorded fingerprint changes. With one system message it still
  is, and an earlier build's digest still replays (C8). With two or more (a seed or a compaction
  summary), the canonical form is the block list actually sent: Anthropic's documentation binds a
  signature to "the top-level `system` prompt" and is silent on whether `"A\n\nB"` and `[A, B]` are
  the same prompt, and a block replayed under a prompt the provider considers rebuilt is a 400 on
  every later request for an enforced account. An earlier build's signed reply in such a session is
  therefore sent neutrally on every later request (`a_joined_system_digest_from_the_previous_build_is_sent_neutrally`).
- **A public evidence hook.** `AnthropicLlm::encode_with_replay_rule` returns the body and the
  replay digest under either fingerprint rule, for C16's replay case. `replay_across_tail` is
  exported.
- **Live results (merge plan step 3).**
  - C16 was not run: it spends Anthropic API credit, which this implementation was not authorised
    to spend. `replay_across_tail` stays `false` for every model.
  - C17, subscription plan, `gpt-5.6-sol`, 2026-10-28: **accepted.** A body with a developer system
    message and no `instructions` was accepted and answered as the prompt directed (`PINEAPPLE`).
    Three managed steps with a trailing developer tail after `function_call_output` were accepted.
    A `nanus run` on the subscription with the new build also completed.
  - C17, subscription cache read, the same day: **failed.** Step 1 read 6,656 of 7,111 prompt tokens
    from cache (written by an earlier run), but steps 2 and 3 read none. In a probe of three
    identical requests per arm, the subscription served from cache irregularly with or without a
    `prompt_cache_key`: developer form 0/3 without a key and 1/3 with one, `instructions` form 1/3
    without and 2/3 with. This is not a carrier refusal, so D6's fallback does not apply. Whether the
    subscription needs `prompt_cache_key` to route a session's requests to its cache was put to the
    owner, who decided that both plans send the session's key (see Owner decision below).
  - C17, API plan (Responses and chat), and DeepSeek: not run (API credit). No path fell back.

## Review (2026-10-28)

A semi-formal review of the implementing commit (57fbfb03) found the following. Each is resolved in
the follow-up commit.

1. **Anthropic breakpoint 2 landed on the tail of a first request** (confirmed by execution). On a
   request whose head is also its final turn — every managed session's first step — the head
   search did not stop short of the tail, so the only conversation entry written ended in a tail no
   later request carries, and breakpoint 4 was dropped behind it. Every breakpoint search now stops
   before the tail; the unshipped `AppendedExplicit` head had the same flaw and is fixed alike.
   Pinned by `a_first_requests_head_breakpoint_stops_before_the_tail` and the explicit fallback's
   first-request case.
2. **`"system": null` on a cached request with no system message** (confirmed). Indexing a missing
   key inserted it. Fixed; pinned by `caching_a_request_with_no_system_message_adds_no_system_field`.
3. **Signed replay against Anthropic's preserved thinking** (confirmed from the provider's
   documentation, not executed). For Opus 5.5, Sonnet 5.5 and Fable 5.1 a thinking block's signature
   binds `system`, `tools` and every earlier message; removing a per-request reminder invalidates
   every later block, and accounts created on or after 2026-08-31 are refused (400) on such a
   replay. So:
   - C16's relaxed rule is not expected to pass for any model while the tail is removed from the
     next request; `replay_across_tail` stays `false`, and its documentation says why.
   - The shipped default is sound under those rules: a reply produced under a tail is sent without
     its thinking (stripping thinking blocks is the documented recovery), and in a managed session
     every reply is, so no invalidated block is ever replayed.
   - Sending such a reply neutrally is accepted: the same documentation gives "strip every
     `thinking` and `redacted_thinking` block from the history (each turn's `text` and `tool_use`
     blocks stay), then retry" as its recovery, so no live run is needed to rely on it. What it
     costs is that reply's reasoning on later requests, which this spec already accepts.
   - **Decision: the append-only alternative is not adopted.** The documentation keeps earlier
     thinking valid by leaving each step's reminder in place — a text block after that step's tool
     results, or a `role: "system"` message with `clear_at: "next_user_message"` (a beta). Either
     would make every step's tail a permanent part of the conversation: recorded in the session so
     it can be re-sent byte for byte, ahead of every later message, and accumulating up to
     `TAIL_BYTES_MAX` per step. That contradicts this change's invariants (the tail is never
     recorded; nothing that changes per step precedes the newest message), so it would be a change
     of its own, with its own spec, if reasoning continuity on Anthropic is ever worth that cost.
   - The multi-message `system` risk is resolved conservatively (see *Deviations as built*).
4. **AGENTS.md and the documentation's introduction understated legacy changes**: Anthropic's
   per-message `system` blocks and the loss of thinking for a legacy reply produced under an
   elision notice. Both are now listed.
5. **A misleading refusal from the provider-aware legacy fit** for a request ending on an answer:
   it read as a budget too small. It now names the missing place for the notice; pinned in
   `an_unconfigured_budget_follows_the_window_of_the_model_in_force`.
6. **Coverage gaps**: a recall search for the tail (C11), a managed-mode link status (C14), the
   model-visible status shape (C14), the idle dry preparation, and the stale profile (C13). Each now
   has a test (below).
7. **Minor**: the interface's cache share is clamped at 100%; the link's usage cache, set as usage
   streams, is documented as such.

## Second review (2026-10-28)

A second semi-formal review, of the cumulative change through 006e473d, found every finding above
resolved and no correctness defect in the core changes. It raised four lesser findings, resolved
in the commit that follows it:

1. **Undocumented and unaccepted wire placements.** The Responses head breakpoint sat on the
   working data's `output_text` part; it is now on the first user item's `input_text` part (see
   *Deviations as built*). The system-part breakpoint, on a developer message's `input_text` part,
   is the placement the spec's reference documents and stays on the API plan, with C17's API-plan
   case still to be run before it is relied on as evidence.
2. **A developer-role tail at an OpenAI-compatible gateway.** It is now appended there.
3. **Documentation inaccuracies**: the tail is harness-written text, not only numbers and ids; a
   joined-`system` reply is sent neutrally on every later request, not once; breakpoint
   candidates are considered in listed order; a cache read can cost less than a tenth.
4. **Weak coverage**: C1 now has a test at the hidden-id cap with every field at its widest
   (`the_tail_fits_its_bound_at_the_hidden_id_cap`), and C13's test stages a proposal under the
   pre-guide profile and sees it refused `stale_base`, beside one under the current profile that is
   staged.

## Coverage

| # | Tests |
|---|---|
| C1 | `compile/tests.rs`: `the_tail_states_every_key_and_the_catalog_only_when_something_is_hidden`, `the_tail_fits_its_bound_at_the_hidden_id_cap`, `a_fresh_request_has_no_prefill_and_points_at_get_goal` |
| C2 | `compile/tests.rs`: `the_working_data_changes_only_with_the_notes_or_the_objective` |
| C3 | each adapter's `layout_tests.rs`: three managed steps, the goal-first exception and a legacy session between two drops — Anthropic; OpenAI chat, Responses on both plans, z.ai and Inception; DeepSeek |
| C4 | each adapter's `layout_tests.rs`: `a_fit_breaks_only_the_body…` |
| C5 | each adapter's `layout_tests.rs` (tail placement) and each adapter's `managed_tests.rs` (`a_request_outside_the_managed_grammar_is_refused` refuses an answer-ending request) |
| C6 | `anthropic/layout_tests.rs`: `managed_breakpoints_follow_the_layout`, `a_first_requests_head_breakpoint_stops_before_the_tail`, `a_breakpoint_at_or_before_one_placed_is_dropped`, `a_breakpoint_walks_back_past_thinking` |
| C7 | `anthropic/layout_tests.rs`: `ordinary_breakpoints_follow_the_layout`, `caching_a_request_with_no_system_message_adds_no_system_field` |
| C8 | `anthropic/managed_tests.rs`: `signed_replay_is_sent_only_under_the_prefix_that_produced_it`, `signed_replay_survives_the_tail_where_the_model_allows_it`; `anthropic/layout_tests.rs`: `a_digest_recorded_by_the_previous_build_still_replays` (one system message), `a_joined_system_digest_from_the_previous_build_is_sent_neutrally`, `a_legacy_reply_under_an_elision_tail_replays_only_where_the_model_allows` |
| C9 | each adapter's `a_managed_request_sends_exactly_the_bytes_it_prepared` (the decorated body, tail and markers included) |
| C10 | `openai/layout_tests.rs`: `the_responses_breakpoints_follow_the_plan_and_the_mode`, `the_explicit_fallback_marks_three_places_and_appends_the_tail`; `tests/ordinary_responses_bytes.rs` |
| C11 | `bundle/tests/managed_context.rs`: `the_guide_closes_the_prompt_and_the_tail_is_never_recorded` (the log, the checkpoint, and a `context_recall` search for the tail that finds nothing beside one for the prompt that does) |
| C12 | each adapter's `layout_tests.rs`: `a_forged_notice_in_a_tool_result_is_content…` |
| C13 | `bundle/tests/managed_context.rs`: `an_adapter_declaring_the_earlier_layout_is_unsupported`, `the_guide_closes_the_prompt_and_the_tail_is_never_recorded`; `bundle/src/agent_loop.rs`: `a_profile_taken_before_the_guide_is_not_the_current_one` (a proposal staged under the pre-guide profile is refused `stale_base`; one under the current profile is staged) |
| C14 | `link/server/context.rs` (`wire_status`), `link/src/protocol.rs` (round trip with and without the fields), `link/tests/link.rs`: `a_status_carries_the_last_requests_cache_share_in_a_legacy_session`, `a_status_carries_the_last_requests_cache_share_in_a_managed_session`, `tui/src/runtime.rs`: the cache-share line and one-line tests, `domain/src/session.rs`: `the_last_reported_usage_is_…`, `domain/tests/context_schema.rs`: `the_model_visible_status_carries_no_cache_share` |
| C15 | `bundle/src/agent_loop.rs`: `an_unconfigured_budget_follows_the_window_of_the_model_in_force` (tail and refusal), `bundle/tests/managed_context.rs`: `a_legacy_session_is_untouched_by_the_managed_entry_point`, `bundle/tests/admission/host.rs`: `projection_keeps_full_history_…`, `bundle/tests/end_to_end.rs`, `bundle/tests/embedding.rs` |
| C16, C17 | `bundle/tests/live_cache.rs` (`#[ignore]`d); results above |
| Idle dry preparation (deviation) | `anthropic/managed_tests.rs`: `an_idle_sessions_next_request_is_measurable`; `bundle/tests/managed_responses_wire.rs`: `status_reports_the_effective_reservation_across_a_switch_and_for_legacy_sessions` (the status after a turn, through the real OpenAI adapter) |
| C18 | `domain/src/context.rs`: `the_fitter_keeps_the_same_turns_it_kept_with_the_notice_as_a_message`, `the_tail_framing_decides_a_boundary_case_each_way`, `the_notice_is_counted_against_the_budget`; `bundle/tests/maintenance.rs`: `a_trimmed_compaction_request_carries_the_notice_as_its_tail` |
| C19 | `openai/managed_tests.rs`, `openai/responses.rs` and `compaction_wire_tests.rs`, `tests/ordinary_responses_bytes.rs`; `stateless_tests.rs` unchanged |

## Owner decision (2026-10-28): the session's cache key on both plans

The owner decided that a subscription request should be no different from an API request, and
that a `prompt_cache_key` is sent. As built:

- `ChatRequest::prompt_cache_key` names a request's cache group. The runner sets it to the session's
  id on every request it builds: legacy turns, managed steps and compaction drafts.
- The `OpenAI` adapter sends it, under any retention but `None`, wherever `CachePath::cache_key`
  says so: Responses at the API and subscription endpoints, and chat completions at the API
  endpoint. A gateway configured as `OpenAI`, z.ai and Inception are sent none, since they document
  no such field. The opt-in stateless replay mode sends none: its receipts seal its body.
- The key is per session, the conventional grain on the subscription backend. Separate keys are
  separate cache groups, so the system-prompt entry is no longer shared across sessions. The live
  case that claimed it was (`a_second_session_reads_the_shared_prompt`) is removed, with its row in
  C17.
- Tests: `the_cache_key_goes_to_openai_on_both_plans_and_nowhere_else`,
  `both_plans_send_the_session_cache_key_when_caching_is_asked_for`,
  `a_request_names_its_session_as_its_cache_group`, and the key asserted on a managed step and a
  compaction draft.
- Live, subscription, `gpt-5.6-sol`, 2026-10-28, after the change:
  - the keyed layout was accepted;
  - the cache-read case was run three times, plus the acceptance case once:
    - twice, step 2 read nothing;
    - once, step 2 read 7,168 of 7,265 prompt tokens (floor 6,912), and step 3 read 6,912 of
      7,419 (floor 7,040);
    - the acceptance case's step 3 read 6,656.

  Reads now happen within a session, but irregularly, so the key does not make the subscription's
  cache dependable. No hit-rate claim is made.

