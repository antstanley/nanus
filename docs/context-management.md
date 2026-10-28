# Managed context

Managed context is an **opt-in, per-session** way of building the request a model is sent. The
legacy fitter drops whole old user turns once a conversation outgrows its budget; managed context
instead keeps **every user message**, selects among the model's own completed work, keeps
bounded source-backed working notes, lets the model read earlier evidence back, and checkpoints
every accepted decision before a request uses it. The raw session log stays the only authority:
the effective request is a derived view, and nothing rewrites, reorders or deletes an event.

A session that never enables it reads, runs and saves exactly as before, with the same tool count
and the same event trace. Its model is sent the same content as before, with two differences in
where things sit, both shared by every legacy session and neither caused by managed context:

- the fitter's elision notice, when turns were dropped, is the request's tail;
- on Responses, the system prompt is a leading developer message.

On Anthropic, `system` is one text block per system message, and a reply produced under an elision
notice is sent without its thinking on later requests (see
[signed replay](#providers)). Its requests also carry cache hints (see
[Request layout and provider caches](#request-layout-and-provider-caches)). Two changes apply to
every session
by design: [deletion](#deletion-and-retirement) retires the id, and the `read` and `grep` tools
gain the [byte-window and coverage](#evidence-recall-and-file-windows) arguments in their schemas;
their results are unchanged when those arguments are absent.

The boundary payloads are defined in [`context-management.schema.json`](context-management.schema.json)
(JSON Schema Draft 2020-12). Schema validity is necessary and never sufficient: every semantic,
encoded-byte, role, reference, quota and provider rule below is checked in code.

- [Turning it on](#configuration-activation-and-limits)
- [Fragments, frontiers and revisions](#fragments-frontiers-and-revisions)
- [What a managed request carries](#what-a-managed-request-carries)
- [The two context tools](#the-two-context-tools)
- [The step transaction](#the-step-transaction)
- [Providers](#providers)
- [Checkpoints and recovery](#checkpoints-and-recovery)
- [Evidence, recall and file windows](#evidence-recall-and-file-windows)
- [Goals and the manual policy](#goals-and-the-manual-policy)
- [Human controls and clients](#human-controls-and-clients)
- [Accounting](#accounting)
- [Errors](#errors)
- [What is and is not verified](#what-is-and-is-not-verified)

## Configuration, activation and limits

`ContextPolicy` has five fields: `mode` (`legacy` | `managed`), `output_reserve_tokens`,
`output_reserve_follows_model`, `capture_shell` and `policy_version`. The defaults are legacy, a
following reservation, false and 1. A *following* reservation is resolved at each request from the
model in force, so it moves with a model or provider switch:

- when requests are fitted to the model's window (the stock default, with no `context_budget`
  configured), it is the model's declared output ceiling;
- when they are fitted to a smaller budget — a configured `context_budget`, or a library host's
  default of 64,000 — it is the smaller of the ceiling and a quarter of that budget, as for image
  requests, so at least three quarters of the budget stays for input.

`output_reserve_tokens` then records the reservation resolved when the policy was recorded, for a
reader, and is not otherwise used. An *explicit* reservation is the recorded number and does not
move. Capture is valid only in managed mode, and nothing turns managed mode on implicitly.

A policy change is a human or host action taken while the session is idle. It is recorded as a
`context/mode` record and checkpointed before it is acknowledged. Enabling makes the body managed
through the checkpoint — version 3, or version 4, 5 or 6 marked managed; a default enable writes version 6, because its
policy follows the model;
disabling never downgrades it and never deletes past records. Activation refuses — without changing the session — when the selected model path has no
managed preparation, when the reservation is explicit and the model's declared output ceiling is below it, or when the
reservation follows the model and the model declares no output ceiling, when a
registered tool already uses a context tool's name, or, for capture, when there is no archive or
the shell cannot capture.

`nanus run` takes `--context-mode legacy|managed`, `--context-output-reserve <tokens>|model` and
`--capture-shell-evidence`. `model` selects a following reservation and a number an explicit one.
Absent flags keep a resumed session's recorded policy, or legacy for a new one. A session recorded
with an explicit reservation keeps it until a human changes it; a new managed session follows the
model. A capture or reserve flag on a new session without `--context-mode managed` is refused.

The policy-1 limits are fixed (`nanus_domain::context::managed::limits`). They are engineering
defaults, not measured optima, and any future tunable keeps an absolute bound inside the snapshot
profile.

| Quantity | Bound |
|---|---|
| Soft-pressure hint | 75 % of the input allowance; re-arms only below 60 %; at most once per four completed model steps |
| Automatic reduction | Acts only above the hard allowance; continues toward 60 % while eligible fragments remain |
| Management | At most one proposal per settled step; every model call charges the step budget |
| `context_manage` / `context_recall` raw arguments | 16 KiB / 2 KiB, checked before JSON parsing |
| Notes | 32 per revision, 512 Unicode scalar values each, 1–4 sources each, 8 KiB encoded array |
| Hidden fragment ids | 4,096 per revision, strictly increasing; reaching the cap refuses further reduction |
| Revision record | 64 KiB encoded |
| Recent protection | The latest two completed substantive fragments, and the newest management fragment until a later completed response consumed it |
| Recovery catalog | 2 KiB |
| Inspect / recall output | 8 KiB encoded; 40 descriptors or hits per page; recall examines at most 256 KiB per call |
| Shell capture | 8 MiB per stream, 128 MiB per session, 1 GiB per store; 128 KiB staged; 5 s sink deadline |
| Records | The existing 4 MiB per record and 64 MiB per session; a closing reserve is kept for every step |

The admissible input allowance is the smaller of the configured context budget and the model's
window, less the output reservation and any separate reasoning reservation, and never above the
model's input ceiling.

With a following reservation and the model's window as the budget, the allowance is the window less
the output ceiling. On every supported model that leaves at least 78,000 tokens (`mercury-2`:
128,000 − 50,000); for OpenAI it is 922,000, the declared input ceiling. With a smaller budget the
allowance is at least three quarters of it.

## Fragments, frontiers and revisions

A **fragment** (`f:<seq>`) is one retained assistant message — the sequence of its event — and
every settled tool result that answers its calls, paired by call id rather than by position.
Text-only assistant messages are fragments too. A fragment is settled once every call is answered
or its turn has ended. Calls and results are atomic: sibling calls are never split and a call is
never shown without its result. Derivation refuses a log whose call identity is ambiguous — an id
carried twice, answered twice, or answered with no call — rather than inventing a grouping.

A **frontier** is a session id, an exclusive event count, the accepted revision, and the BLAKE3
of the stored header and the first `event_count` event lines, byte for byte as the session
serializer writes them (`Session::prefix_digest`). At the full count it is the digest of the
whole stored file, which is what lets a store recompute it from disk without re-encoding.
The header names the body's version, and a body moves version when it gains a record an older
reader cannot hold — typed user content moves it to 4, a handoff draft's attempt record to 5. A
frontier taken before such a move still describes its prefix, so a frontier is checked against the
header the body has now and, failing that, against the header it had at each earlier version
(`check_frontier`); a digest that matches none of them is stale, so an accepted revision outlives a
version move and a changed prefix is still caught.

A **revision** stores the *full* hidden set and the *complete* note array, never a patch. Revision
zero is the raw selection; each accepted revision is its base plus one, and a reset advances the
counter rather than restarting it. A reset is a barrier: what came before it is read only for the
facts a reset preserves, so a session whose earlier projection is invalid can be reset without
that selection ever being executed. Decision ids are host-generated and a decision is accepted
at most once.

The **snapshot profile** binds the selection identity and epoch, the system-prompt and offered
schema digests, the policy and the goal revision. A proposal echoes its digest; any change to
model, effort, schemas, policy or goal since the inspection makes the proposal stale.

## What a managed request carries

Compilation (`derive_effective_context`) is fixed in order:

1. Fold the raw replay, derive the fragments, and check the accepted revision against its base.
2. Keep every original user message and every protected fragment; hidden ids remove whole
   eligible fragments and nothing else; what survives keeps its original order.
3. Render the accepted notes and the goal objective as one labelled **assistant** text message,
   with no calls, reasoning or replay, immediately after the earliest user message. Its text
   changes only when the note array or the goal objective changes. Revision numbers, the goal's
   phase, revision and provenance, note staleness and the hidden-history catalog are stated in the
   tail, never here. A session with neither notes nor a goal has no such message, even when
   fragments are hidden.
4. Never make that message the last one: a fresh request with nothing after its first user
   message carries no generated data, and the tail says `goal_data_available=false`.
5. Render the **tail**: the fixed-schema notice line and, when anything is hidden, the bounded
   recovery catalog. The notice line states the counts, revision, estimate and its estimator,
   allowance, pressure, the goal's phase, revision and provenance, note staleness, and recovery
   availability. Only numbers, enums and opaque ids appear in it, never anything retrieved or
   written by a model. The tail is not a message. It travels in `ChatRequest::context_tail`, and
   the adapter places it after the newest message (see
   [Request layout and provider caches](#request-layout-and-provider-caches)). It is never
   recorded in the session log, so recall never finds it and no fragment contains it.

A managed request has three parts, in this order:

| Part | Holds | Changes when |
|---|---|---|
| Head | Tool schemas; the system prompt with `MANUAL_POLICY_V1` and `MANAGED_REQUEST_GUIDE_V1`; a seed; the first user message; the working-data message | The model, effort, schemas, policy, seed, note array or goal objective changes |
| Body | The selected conversation, in event order | It grows by appending. It changes in place only when a revision hides or restores a fragment, and then only from the earliest fragment the revision changed |
| Tail | The notice line and the catalog | Every request |

**Nothing that changes from one step to the next comes before the newest message.** Take two
consecutive requests of a session with no revision, goal, policy, schema, model or effort change
between them. Remove the tail and the cache markers from both. The earlier request's encoding is
then a prefix of the later one's.

There is one exception. When notes or a goal exist before a session's first prompt (a goal set
with `/goal`, or a seeded session's notes), the first request has no working-data message,
because it would be last. The second request inserts it after the first user message. Every
adapter is tested for the rule and for this exception.

Deterministic hard fitting acts only when the candidate is over the hard allowance, hiding the
oldest eligible complete fragment first. Every probe is compiled and prepared by the selected
adapter, tail included, so its cost is the cost of the body that adapter would send. A protected
floor above 60 % but within the allowance succeeds; above the allowance it refuses with
`protected_floor_too_large`, and no partial hidden set is installed. A successful fit is an
automatic revision persisted with the request intent before dispatch. The catalog is in the tail,
so a fit that changes no notes leaves the head unchanged. After a reset to legacy, the legacy
whole-turn fitter applies again. Its notice is that request's tail, placed the same way.

An idle session ends with the model's answer, where no tail can go. A status read or a staged
proposal's revalidation measures the request the next prompt would start, so it is prepared with an
empty prompt standing in for the one not yet typed; nothing prepared that way is ever sent.

## The two context tools

`context_manage` and `context_recall` are offered beside the seven registered tools and the five
goal tools only in a managed session whose model path supports them. The registered count stays
seven, a legacy session's schemas are unchanged, and the encoded definitions carry only the three
allowed fields.

- **`context_manage inspect`** returns the context status (revision, frontier, profile digest,
  pressure, last decision) and a page of fragment descriptors, sized to 8 KiB. Its cursor is
  sealed with a process-held key and bound to the revision, frontier and profile.
- **`context_manage propose`** names its base revision, frontier and profile digest, hide and
  restore deltas, and a complete replacement note array. It is validated against the complete
  snapshot, staged, and returns `staged` — never "committed".
- **`context_recall search`** finds a literal, case-sensitive string in the session's neutral
  text and published archive objects; **`read`** returns one bounded range of one exact source
  with the digest of the whole source, for citing in a note.

Neither tool can reset context, enable capture, change policy, delete an artifact, select another
session or path, or change permissions. A note reference only names evidence.

Unlike the goal tools, context calls have **no policy bypass**: each is shown to the host
`ToolPolicy` with an access descriptor (`read` for inspect and recall, `write` for a proposal);
the stock default grants this session's own reads and projection writes, and a host policy may
deny either. They go through complete-batch admission and `before_dispatch` like any call.

A proposal must be the only call in its message. A batch that mixes one with any other call is
refused whole — every call answered with `mixed_mutation_batch`, in model order — before any
approval, goal change or executor runs.

## The step transaction

```text
admit the human turn -> append TurnStart/UserMessage
hold the exact selection
prepare the effective request -> optional automatic revision
reserve record capacity
checkpoint settled prefix + automatic revision + RequestAttempt(started)
append StepStart -> dispatch the frozen call -> append the assembled observation
classify mixed proposals -> policy/approval -> admission -> execute -> ordered append
append StepEnd -> evaluate a staged proposal
a proposal: checkpoint the candidate revision or the unchanged one + RequestAttempt(finished)
none: append RequestAttempt(finished) for the next checkpoint to carry
install what was acknowledged -> honour cancellation -> release the selection
next step, or append and checkpoint TurnEnd before the answer is reported
```

A step that staged no proposal is not checkpointed on its own. Nothing leaves the process between
its settlement and the next checkpoint — the next step's intent, or the turn's end — so one commit
carries both, and a turn of `n` steps takes `n + 1` checkpoints rather than `2n + 1`. A refused
commit leaves the session holding exactly what a refused settlement would: the finished attempt,
which both outcomes record. A step that staged a proposal still settles in a checkpoint of its own,
because its revision must be acknowledged before the next request is prepared under it.

The selection is held whenever managed mode or either admission port is active; setters queue
and apply after the step. The adapter-owned prepared call is frozen once: its body digest is
recorded in the intent and the same object is dispatched, never re-encoded or re-resolved.

A staged proposal is re-validated after `StepEnd`: the snapshot compare-and-set again, its
references, the record bound, and a dry preparation proving the *next* request would fit. An
oversized proposal is rejected as it stands — nothing is hidden on its behalf. Acceptance records
the revision and `context/decision accepted` together in one checkpoint; a rejection keeps the
previous revision and records the code. A proposal whose validation had not begun when the turn
was stopped is recorded `cancelled`. Executed effects and failed calls are never rewritten.

## Providers

Managed support is an explicit adapter capability (`LlmPort::managed_support`), unsupported by
default. `LlmPort::prepare_managed` returns a `PreparedModelCall` that owns the admitted body and
its endpoint and credential privately, exposes its estimate, digest and selection identity, and
performs no I/O until `stream` is polled. A managed estimate charges the serialized body at
three bytes per token (`MANAGED_BYTES_PER_TOKEN`, estimator `adapter-serialized-body/3`) where
an ordinary request is charged one: still above what the providers' tokenizers count, but not
the fourfold overestimate that hid history the model had room for and made the protected floor
— every user message — run out at a quarter of the budget. Ordinary requests are measured
exactly as before. Each step prepares twice: once as the fitter's probe, whose cost the frozen
request's tail reports, and once to freeze it. The runner's own call assembler and the adapters'
decoders apply the context tools' raw argument limits once a call's name is known — including a
name that arrives after its arguments — and before anything is parsed.

The supported matrix is recorded with the provider work (see [Providers](#provider-matrix) below).
The chat-completions, Messages and Responses wires are supported at their official endpoints for
known models. All of them send history plain. Anthropic's may also send signed replay, under the
rule below. The opt-in stateless Responses replay mode stays unsupported. Activation and provider
switches refuse it before HTTP, with no wire, model or effort switch, and no opaque item is
deleted or rewritten.

A model's limits — window, input ceiling and output ceiling — are the model's. They are declared
once per model id and apply on every plan and endpoint that serves it, so a subscription has the
same budget as API access to the same model.

A model or provider switch to an unsupported path makes the session unready; the next request is
refused before HTTP, the shared selection is not reverted, and the host chooses a supported
selection or resets the session's context.

### Provider matrix

Each supported path splits its dispatch into one encode and a separate send. The ordinary path
sends what an ordinary request of the same build sends, and the prepared call sends exactly what
it estimated and digested.
Support requires the official endpoint and a known model; anything else is unsupported.

| Provider | Endpoint and plan | Protocol label | Models | Support |
|---|---|---|---|---|
| DeepSeek | `https://api.deepseek.com` | `deepseek.chat` | `deepseek-flash`, `deepseek-v4-pro` | Supported |
| OpenAI | API plan, `https://api.openai.com/v1`, exact Chat Completions preference | `openai.chat` | The offered models whose chat tool support is not refused (`gpt-6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`) | Supported |
| OpenAI | API plan, `https://api.openai.com/v1`, automatic routing or an exact Responses preference | `openai.responses` | Every offered model | Supported |
| OpenAI | `subscription` plan, `https://chatgpt.com/backend-api/codex`, automatic routing with the Responses fallback | `openai.responses` | Every offered model | Supported; the reservation is enforced by nanus's accounting and not sent |
| OpenAI | `subscription` endpoint under any exact preference, or an automatic preference whose fallback is chat | — | All | Unsupported: an exact preference promises a ceiling the endpoint refuses, and the chat fallback would send one |
| OpenAI | Stateless Responses replay | — | All | Unsupported; refused before HTTP |
| OpenAI | Any id the vendor does not offer, or any other endpoint | — | — | Unsupported |
| z.ai | API plan, `https://api.z.ai/api/paas/v4` | `openai.chat` | The known API models | Supported |
| z.ai | Coding Plan, gateways | — | — | Unsupported |
| Inception | API plan, `https://api.inceptionlabs.ai/v1` | `openai.chat` | `mercury-3`, `mercury-2.5`, `mercury-2` | Supported |
| Inception | Gateways, unknown ids | — | — | Unsupported |
| Anthropic | `https://api.anthropic.com/v1` | `anthropic.messages` | `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-fable-5-1` | Supported |

Inception's row rests on the same local preparation and reload fixtures as the other chat
providers, including the exact body the prepared call sends. Like every provider in this table it
has **not** been sent a live managed request; [what is and is not verified](#what-is-and-is-not-verified)
records that limitation for all of them.

The output reservation must be present and within the model's declared ceiling; it is refused,
never clamped. Every adapter validates the managed role grammar — leading system messages only
(the system prompt and a seeded session's handoff seed), the generated message, when present,
directly after the first user message and never last, every surviving call paired with exactly one
result, and a conversation that ends with a user message or tool results, which is where the tail
goes. Generated data is identified by that
position; a model reply elsewhere that opens with the same label is sent as the reply it is.
Under managed preparation the chat decoders also bound a call's arguments before its name is
known, at one record.

Anthropic signed replay is sent only when the adapter's existing check accepts it: the original
blocks under the encoded prefix that produced them. The prefix fingerprint never includes cache
markers.

- **By default it includes the tail.** A step's tail is gone from the next request, so a managed
  turn's thinking is not replayed after the step that produced it. That is no loss against what
  managed sessions got before. The same holds for a legacy reply produced under an elision notice:
  it is sent neutrally on later requests, where before this change it was replayed until the
  fitter's next drop. A legacy reply produced with no elision has no tail and replays exactly as
  before.
- **For a model whose live acceptance is recorded** (`replay_across_tail` in the adapter's model
  table), the fingerprint excludes the tail, and signed blocks survive from one step to the next.
  Anthropic's preserved-thinking rules for Opus 5.5, Sonnet 5.5 and Fable 5.1 say a block is bound
  to every message before it and that removing a per-request reminder invalidates it (a 400 for
  accounts created on or after 2026-08-31), so no model is expected to qualify while the tail is
  removed; the documented append-only form keeps earlier reminders in place.

The fingerprint binds `system` as the joined string the previous build sent when there is one
system message — the same prompt as the one block now sent — and as the block list when there are
two or more (a seed or a compaction summary), since whether a signature binds the two forms alike is
not documented. A reply an earlier build recorded under a joined multi-message `system` is
therefore sent neutrally on every later request rather than risk a refusal.

When hiding changes a turn's prefix, that turn is sent in its neutral form — its text and tool
uses, without the thinking blocks — which the Messages API accepts as an edited history. A turn
sent neutrally is sent the same way on every later step, so the cache is unaffected either way.
Signatures and digests are never rewritten. A turn that carries only signed replay and has no
neutral form refuses preparation.

These rest on wire and reload fixtures — a local server receives exactly the prepared bytes — not
on live provider evidence; see [what is verified](#what-is-and-is-not-verified).

#### The Responses wire

A managed Responses request is encoded by the same encoder as an ordinary one. The two differ only
in what the managed compiler selected, the tail, and the head breakpoint:

- The leading system messages (the harness prompt and a seeded session's handoff seed) are the
  first input item: one `developer` message with one `input_text` part each, in order. The body
  carries no `instructions`.
- The tail is a trailing `developer` message item, after the final user item or
  `function_call_output`, and nothing follows it.
- On the API plan, the system prompt's part and the first user item's last `input_text` part each
  carry an explicit cache breakpoint.
- A user turn is a `user` item, with `input_image` parts when it carries images.
- An assistant turn is an `assistant` item of `output_text`, then one `function_call` item per
  call, keyed by `call_id` and carrying no item id.
- A result is a `function_call_output`. A result that carries images is answered with its label,
  and the original text and pixels follow as one `user` item once the whole group of results has
  been answered.
- The generated working-data message is an `assistant` item directly after the first user item,
  and never last.

History is always plain. The body carries `store: false` and never `previous_response_id`,
`include`, a reasoning item, or any other protocol's replay. An assistant turn recorded with replay
blocks (Anthropic's, or stateless Responses items) is sent as its text and calls. A turn that only
replay can express refuses preparation with `protocol_incompatible`, as it does on chat
completions. Earlier reasoning is therefore never sent back to the model. The ordinary Responses
path already works this way, so managed context costs no continuity the session had.

On the API plan the reservation is sent as `max_output_tokens`. The `subscription` endpoint
refuses that field, so there the reservation is enforced by nanus's accounting alone. It must
still be present and within the model's declared ceiling. The estimator charges it against the
budget as on every other path, and the input allowance is reduced by it. With the default
following reservation and the model's window as the budget it is the model's whole ceiling, so
nothing the backend can return exceeds what was reserved. A smaller reservation — explicit, or a
following one bounded by a configured budget — is not sent either. A reply longer than it is
recorded at its real size in the attempt's usage, and the next step's fit counts it as history.
The subscription endpoint under an exact preference is unsupported, because an exact preference
promises that a ceiling is sent and honoured.

The decoder applies the context tools' raw argument limits on this wire too:

- It checks a call once its name is known, whether the name arrives in
  `response.output_item.added` or only in `response.output_item.done`.
- Before the name is known, it bounds the buffer at one record.
- A call over its limit keeps `OVERSIZED_ARGUMENTS`, and the whole arguments string an item's
  `done` event carries cannot restore it.

The selection identity names:

- provider `openai`;
- the digest of the exact `/responses` URL;
- protocol `openai.responses`;
- the model;
- the effort the body's `reasoning.effort` carries.

A prepared call holds the bearer token and account id captured when its adapter was built, as an
ordinary dispatch does, and its `Debug` renders neither. A token that expires between
composition and the send fails that attempt, and renewal happens when the adapter is next built.

A managed session that moves between `openai.chat` and `openai.responses` needs no reset, because
both send plain history. Stock routing never makes that move, since every offered id resolves to
Responses. A library host can make it by preparing with a differently configured adapter. Each
step's selection is recorded with its attempt.

### Request layout and provider caches

Every adapter encodes a request in two steps:

1. **Canonical encoding** is the body the wire would carry with no tail and no cache hints.
2. **Decoration** adds the tail and the wire's cache hints. It is a pure function of the
   canonical body, `context_tail` and `prompt_cache`.

A message encodes to the same bytes whether or not it is the newest one, apart from the tail's own
bytes. The request digest (`PreparedModelCall::request_digest`), the estimate and the dispatch all
read the decorated body, which is what is sent. The Anthropic signed-replay prefix digest reads the
body without cache markers, and without the tail when the model allows it.

| Wire | Where the system prompt goes | Where the tail goes | Cache hints |
|---|---|---|---|
| Messages (Anthropic) | The top-level `system` field: one text block per system message, in order | Its own `text` block, the last block of the final `user` turn, after any `tool_result` blocks | Up to four `cache_control` breakpoints |
| Responses at OpenAI | A leading `developer` message: one `input_text` part per system message, in order | A trailing `developer` message, after the final user item or tool results | Up to two explicit `prompt_cache_breakpoint`s on the API plan: the end of the system prompt and the end of the head |
| Chat completions at OpenAI | Leading `system` messages, as before | At OpenAI's endpoint, a trailing `developer` message, after the final user message or tool results; at a gateway or local server configured as OpenAI, appended as below | None |
| Chat completions at DeepSeek, z.ai and Inception | Leading `system` messages, as before | Appended to the final message's content: a trailing `text` part when the content is a list, otherwise after a blank line in the content string | None |

Both modes have a tail:

- **Managed:** the notice line and the catalog, on every request.
- **Legacy:** the whole-turn fitter's elision notice, only on a request that dropped turns.

Every request a runner builds ends with a user message or tool results. Every adapter's managed
grammar check refuses a request that does not, and the legacy builder refuses to attach a tail to
one. So there is always a place for the tail. The tail is identified by position:

- **Anthropic:** the last block of the request.
- **OpenAI:** the trailing developer message.
- **Other chat vendors:** the text after the final message's own content.

A `[nanus context]` line anywhere else, inside a tool result for example, is that message's
content.

**Retention.** `ChatRequest::prompt_cache` is `None` unless the runner opts in, so a library host
that builds a `ChatRequest` itself pays no cache-write premium it did not ask for. The stock runner
opts in:

- `Short` for `nanus run`;
- `Long` for an agent served over the link, where a person may pause between turns.

Under `Short` every breakpoint is five minutes. Under `Long` every breakpoint is one hour. A cache
write costs 1.25 times the input price for five minutes and twice the input price for one hour. A
cache read costs a tenth of the input price or less (a twentieth on Opus 5.5), so a five-minute
entry pays for itself on its second read and a one-hour entry on its third.

**Anthropic breakpoints** are computed over the encoded `messages` array:

1. the first `system` block, which is the harness's system prompt. A seed or a compaction summary
   follows in later blocks, so the entry is shared by every session with the same model, tools and
   prompt;
2. the last block of the head: the working-data message when present, otherwise the first `user`
   message;
3. the last block of the message immediately before the last `assistant` message. That is where
   the previous request wrote its entry, so the next request reads it even when more than twenty
   blocks were appended in between;
4. the last block before the tail, or the request's last block when there is no tail.

A breakpoint is never placed on a `thinking` block, nor on the tail; the search walks back past
them. Candidates are considered in the order listed, and one at or before a breakpoint already
placed is dropped — as breakpoint 3 is when it would fall on or before the head, which covers it.
So there are at most four, in position order, and their TTLs never increase from start to end. Breakpoint 2 exists only
in managed requests. A prefix shorter than the model's minimum cacheable length is simply not
cached.

**OpenAI.** On GPT-5.6 and later, OpenAI places an implicit breakpoint at the end of the latest
eligible message: a user message, or the last tool response of a group. A trailing developer
message is not eligible, so the implicit breakpoint lands before the tail. The next request finds
that entry among the earlier eligible message endings it looks up.

On Responses, the system prompt is a leading developer message rather than top-level
`instructions`. `instructions` cannot carry a breakpoint, and a developer message part can. On the
API plan, when the runner opted into caching, two parts carry
`prompt_cache_breakpoint: {"mode":"explicit"}`:

1. the leading developer message's first part, which is the harness's system prompt, in both
   modes. A seed or a compaction summary follows it in later parts, so the breakpoint falls before
   anything session-specific. A session's requests write and read this entry, so a seed, a
   summary or a change to the head after it still reads the prompt from cache;
2. in managed requests only, the head's last `input_text` part, which is the first user item's.
   The working-data message after it is an `assistant` item whose part is `output_text`, which no
   breakpoint is documented on, so it is re-read on every step. A fit that leaves the head
   unchanged still reads the entry.

These are OpenAI's first two explicit breakpoints, which it always looks up. The implicit
breakpoint stays on and covers the growing conversation.

The opt-in stateless replay mode keeps `instructions`. Its sealed receipts bind the instructions
in that form, and it is not a managed path; an elision notice joins its `instructions` after the
prompt, as that mode always sent it.

The session's id is sent as `prompt_cache_key` whenever the runner opted into caching, on
both plans and on both wires at OpenAI's own endpoints: a plan changes who pays, not how a
session's requests reach its cache. On models before GPT-5.6 a stable key is how related requests
are routed to the same cache; on later ones routing is automatic and the key separates cache
groups. So one session's requests are kept together as its conversation grows, and a session does
not read another's entries, the system prompt's included. A gateway configured as OpenAI, z.ai and
Inception are sent no key, and the opt-in stateless replay mode sends none.

Each path's carriers are a row of the vendor table (`OpenAiConfig::cache_path`): `system_carrier`,
`tail_carrier`, `system_breakpoint` and `head_breakpoint`. A path that refused a trailing developer
message would take the `AppendedExplicit` carrier instead: `prompt_cache_options: {"mode":"explicit"}`,
every tool output as an `input_text` list, explicit breakpoints at the head, before the last answer
and before the tail, and the tail appended as the final part. No shipped path uses it.

**DeepSeek** writes a cache unit at the end of the request's input and another at the end of its
output, and it matches only whole units. Both of a step's end units contain that step's tail. Its
hits therefore come from common-prefix and fixed-interval units, about one step behind a
tail-free conversation. That is still most of the prompt, where before it was almost none.

**z.ai and Inception** caching is not established here. The tail costs them nothing they had.

## Checkpoints and recovery

A managed turn saves through a `SessionCheckpoint` the host binds to its writer claim, store and
session (`StoreCheckpoint` in the stock composition). A commit names the stored identity it
replaces — the actual file's body version, digest and event count, never a re-encoding — and
returns a receipt whose frontier covers the whole written file.

A commit costs what the step added rather than what the conversation has grown to. The session
carries its encoding forward (`Session::encoded_lines_from`, and the running BLAKE3 states behind
`prefix_digest` and `body_digest`), so only events appended since the last commit are encoded.
The stock store remembers the identity it last wrote with the file's length, time and inode, and
reads the file back only when those have changed. It writes the new file as a clone of the stored
one — copy-on-write where the filesystem has it — with the new lines appended, synced and then
renamed, so a crash still leaves the old file or the new one and never a torn tail.

| Outcome | Meaning | What the turn does |
|---|---|---|
| Acknowledged | The candidate is on disk | Installs it; watchers move their watermark |
| `NotCommitted` | The previous file is intact | Leaves the projection uninstalled, stops new effects, makes one reserved terminal attempt |
| `CommitOutcomeUnknown` | Replacement may have happened | Reads the disk back under the same claim: the candidate installs, the previous file keeps the old projection, anything else freezes the session |

`run_turn_with_runtime` returns the ordinary result **and** a `PersistenceState`, on failure too.
Hosts never save a managed session themselves: `Acknowledged` permits no duplicate save,
`Unsaved` permits nothing beyond the runner's one terminal attempt, and `Unknown` permits only
reconciliation. Checkpoint success guarantees process-crash atomic replacement; the stronger
power-loss grade is not claimed.

A resumed session with an open turn is closed before new work is admitted: an `context/recovery`
record, a finished `unknown_dispatch` attempt for every intent with no outcome, and an interrupted
`TurnEnd`, checkpointed. Nothing is rerun, and an unknown dispatch is never counted as paid usage. A
handoff draft's intent left with no outcome is closed the same way but on its own, under a recovery
record of reason `settled_maintenance_attempt` and with no `TurnEnd`: the draft ran outside any
turn, so it is never swept into the next turn's recovery and charged to it. Recovery runs before
every managed turn and every handoff draft, so a dangling intent is settled before anything new is
admitted. A crash during a tool batch can leave effects newer than the last checkpoint; this design
does not promise exactly-once external effects.

Version 3 adds six records — `context/mode`, `context/revision`, `context/decision`,
`artifact/published`, `request/attempt`, `context/recovery` — each validated on write and on read.
A managed session is version 3, or version 4, 5 or 6 with `"managed":true` in its header when it also
holds typed user content or a maintenance record. A version-6 body always holds a `context/mode`
record, which only `change_policy` writes after upgrading the body, so its header always says
`"managed":true`. Version 5 is written whenever a body carries a
record of a maintenance request: a `context/compact` marker, a `context/seed`, a
`context/handoff`, or a `request/attempt` with a `kind` (what a handoff draft writes). A reader
that predates those would fail part-way through the body, so it refuses the session at the header
instead; those records, and an attempt's `kind`, are refused in any body below version 5, on write
and on read. Readers accept versions 1 to 6; a body that is not managed and carries one of the
managed records is refused, and a future version is refused before anything is mutated.

Version 6 is the lowest version that holds a `context/mode` record whose policy follows the
model. A reader that predates version 6 refuses such a session at the header, and the field's
presence — with any value — is refused in any body below version 6, on write and on read. A
compaction marker's prefix digest is computed under the version-5 header whatever version the
body later reaches, so a compacted session that gains a version-6 record still validates.

## Evidence, recall and file windows

Recall is bound to the session the runner holds at a step boundary, up to its frontier. It reads
only neutral text — user text, assistant text and reasoning, tool text, tool text blocks — and
published archive objects, never a serialized envelope or opaque provider replay. Work is bounded
by bytes examined as well as by hits; reaching a bound returns `coverage: partial` with a cursor,
even with no hits. A missing or corrupt object is reported as such and never replaced by what the
workspace holds now. Sources newer than the last acknowledged checkpoint are labelled
`durable: false`.

Shell capture, when enabled, feeds the exact pipe bytes of `bash` calls to bounded, call-scoped
archive sinks before the preview cap, and publishes an `artifact/published` receipt after the
call's result. See [Shell archives](#shell-archives).

The `read` tool gains a byte-window mode (`byte_offset`, `max_bytes`, optional `version`)
mutually exclusive with the line window, reading at most 64 KiB through an opened, confined
handle and rendering at most 8 KiB, with the file's identity and the range's digest, and a
report when the file changed since an earlier window. Files over 4 MiB, which the line window
refuses, are readable this way. `grep` pushes its include filter into the port before the match
cap and reports skipped-large, binary and unreadable counts with partial coverage.

### Shell archives

`ShellPort::run_with_capture` runs a command exactly as `run` does — the same process groups, Job
Objects, timeouts and cleanup — while each pipe pump forwards the exact bytes it reads to that
stream's sink before the preview cap. Each sink lives in its own task; the pumps share at most
128 KiB of staged bytes, wait at most a second for room, and then stop capturing that stream while
they keep draining for the preview, so capture can never block a pipe or change an outcome. A
write that misses its five-second deadline is never abandoned mid-write: the task keeps the sink
until it is quiescent and finalizes with the truthful reason.

The store's archive (`JsonlStore` implements `ArtifactStore`) reserves quota before a call runs —
8 MiB per stream, 128 MiB per session and 1 GiB per store, counting live reservations, partial
files and orphans — under a cross-process quota lock that is only ever taken after the session
claim. A finalized object is flushed, synced and renamed before its receipt says it exists; its
receipt records its length, its BLAKE3 and one per 64 KiB chunk, so a range read verifies only
the chunks it touches. A reservation that fails becomes an `unavailable` receipt and changes
nothing about how the command runs. A checkpoint that newly references an object verifies it on
disk first. Objects referenced by a receipt are never evicted; garbage collection claims an idle
session, validates its log, and removes only unreferenced objects with no live lease.

The `bash` tool names each archived stream to the model on one bounded line after its preview, for
example `[stdout archived as a:… — 70000 of 70000 bytes, complete; read it with context_recall]`.
With no lease — every legacy session, and every managed one without capture — its output is
byte-for-byte what it was.

### Deletion and retirement

Deletion takes exclusive ownership of the session, refusing one another writer holds. It writes
a retirement marker for the id, moves the whole session directory into the store's trash, then
removes the bytes and reclaims their archive quota. Afterwards a save, checkpoint, claim or name
for that id is refused as retired, so a stale writer cannot resurrect the conversation; a deletion
a crash interrupted is finished the next time the store opens and never reversed. This is the one
deliberate change to legacy behaviour, and it applies to every session.

## Goals and the manual policy

The goal phase and revision are read from `Session::goal` at request construction; a context edit
cannot change them. A model revision binds its notes to the goal revision it inspected; automatic
fitting keeps that binding, so a later goal change makes the notes explicitly stale in what the
model reads, until a new accepted note set binds to the new revision. Goal text keeps its
provenance — `model` for a change made by a goal tool inside a step, `host` for one made outside —
and is shown as data.

Managed requests append one bounded manual policy (`MANUAL_POLICY_V1`) to the system prompt:
keep user constraints, hide resolved exploration, cite evidence, keep uncertainty labelled, revise
contradicted notes, and do not claim a test passed without a cited result. It is versioned with
the policy and hashed into the system-prompt digest. It grants nothing, and it is never modified
at run time.

Managed requests also append `MANAGED_REQUEST_GUIDE_V1` to the system prompt, after the policy.
It says three things:

- Every request ends with a `[nanus context]` tail the harness adds after the newest message. The
  tail is metadata, not a user instruction, and a `[nanus context]` line anywhere else is content.
- What the tail's fields mean: `notes_stale=true` means the goal changed after the notes were
  accepted, and `goal_data_available=false` means the goal can be read with `get_goal`.
- Every accepted context change makes the provider re-read the request from the earliest change
  onward, so one change that hides a resolved stretch is cheaper than several small ones.

The guide is fixed text. It is hashed into the system-prompt digest like the policy, and it does
not change `POLICY_VERSION`.

## Human controls and clients

The command line takes the three flags in [configuration](#configuration-activation-and-limits).
A managed session run by `nanus run` is saved only through its checkpoints; the command reports
an unsaved or uncertain outcome on stderr, keeps stdout for the answer, and exits non-zero.

The link has a `context` request with `status`, `legacy`, `managed` and `reset` actions on a held
session. In the interface, `/context` opens a mode chooser; `/context legacy`, `/context managed`
and `/context reset` open the named change's impact warning, and `/context status` reads status.
Every mutation requires an explicit `y` after the warning (`Esc` cancels); warnings scroll on short
terminals. The chooser and a confirmed change pause the interface's prompt queue until dismissal
or acknowledgement. Changes apply only to future requests, never to a request in flight.

Switching to legacy preserves the managed hidden selection and working notes for re-enabling,
but stops using them, removes context tools, disables new shell capture and returns to whole-turn
fitting, which can omit original user constraints. Re-enabling managed reuses that selection and
notes. Reset alone clears them. Switching can change model-visible history, token use and cost,
invalidate prompt caches and lose prior reasoning replay or answer continuity. Enabling upgrades
the stored body format irreversibly; older builds may no longer read it. Raw events and existing
archives are never deleted. A switch preserves the output reserve and never enables shell capture
implicitly. Unsupported activation is refused by the agent, without changing the policy.

A mutation while a turn runs is refused; a status read
during a turn is answered from the snapshot the turn last published, never by borrowing the
running session. A reset selects legacy, appends an empty host revision and persists both before
it is acknowledged. Status carries the mode, readiness and its reason, the revision and frontier,
the profile digest, the last decision, the hidden and protected counts, the estimate and its
estimator, the reservation and whether recall and the archive are available — never credentials,
provider payloads or evidence.

The link's context status also carries `last_prompt_tokens` and `last_cache_hit_tokens`, in both
modes. They are read from the usage of the newest assistant message that reported usage, and are
null when none has. Zero cache-hit tokens means the provider reported none or reported no cache
counter at all, because the neutral usage record cannot tell those apart. These fields are on the
link's `ContextStatusInfo` only. The domain `ContextStatus`, which the model reads in
`context_manage` results, does not carry them.

`ContextStatus`, `ContextDecision` and `Checkpoint` frames carry the session id, the stream epoch,
the turn and step (null when idle), a frame id assigned by the session, the stream watermark and
the durable frontier. Attachment and backlog semantics are described in
[sessions](sessions.md#what-travels-over-the-link); the link protocol is version 12. The interface
draws context notices with the replay's own wording, so a watched turn and the same turn read
back say the same things. A managed session whose store cannot checkpoint, or whose recovery or
reconciliation could not be settled, is held but takes no turn until it is reset.

## Handoff

A handoff starts a new managed session from a summary of this one, then clears the interface onto
it. A person asks for it at the interface (`/handoff`, with an optional name for the new session),
reads the draft, and confirms it. It is offered only for a managed session: a legacy session has no
attempt records or checkpoints to write the draft through, so a handoff there is refused with
`handoff_unavailable`. The managed selection is also what the new session starts from, so the
handoff describes the state the selection holds rather than a prompt the fitter has already cut.

The document is generated data, not a message. It has two parts, and the first is never written by
a model. The header holds the facts the log already has: the source session and the frontier the
draft was drawn from, the goal with its phase and revision, the context mode and policy, the model
and effort, the workspace and the date. Its field lines are fixed (`source_session:`,
`source_frontier:`, `goal:`, `context_mode:`, `model:`, `workspace:` and `date:`), and a narrative
line that opens with one of those names, after leading whitespace and in any case, is refused, so
the model cannot pass its own text off as a header. The narrative is one bounded response to a fixed
instruction set: keep every user constraint, drop duplicates, state any resolution of a conflict as
the model's resolution, record decisions, state, open items and next steps, and cite evidence only
by its position in the log. The whole document (header and narrative) must be within 32 KiB and pass
the same text rule that compaction uses, so a narrative that is empty or only whitespace, has a
`[nanus` line, a carriage return, a control character or an invisible formatting character (a
zero-width space, a byte-order mark, a bidirectional control) is refused, never repaired. A
zero-width joiner or non-joiner is allowed, since emoji and some scripts need one, but the check of
how a line opens looks through them, so neither can hide a label or a header field. The narrative
has its own bound of 16 KiB. The header, the narrative's bound and the rule against imitating a
header line are checked again whenever the seed is written or read (`HandoffSeed::validate`), so a
seed edited on disk is refused rather than sent.

### Drafting writes attempt records and nothing else

A draft is one model call, made through the managed step transaction like a turn's request. The
frozen request's intent is checkpointed before anything is sent, with `kind: "handoff"` on the
attempt. Its finished record follows with the outcome: `completed` when the narrative was accepted,
`refused` when the response asked for a tool, broke the text rule or imitated the header, or stopped
at the output ceiling, `cancelled` when the person interrupted it, and `failed` when the stream
failed. A document that would be over its bound once the header is added is recorded `refused`
too: the header's length is known before the outcome is written. Those two records are the only
records a draft writes, and every viewer of the source is sent a `Checkpoint` frame for them, as
for any idle checkpoint. Before the call, the draft closes what a crash left open — a turn with no
end and a handoff intent with no outcome — the way a turn does. A draft is interrupted the way a
turn is, but only by the connection that asked for it: the link reserves the session before it
answers the request and runs the draft in a task of its own, so that connection's `Interrupt` is
read while the model is answering and stops the call at once, and one that arrives before the call
has started stops it before the model is asked; the draft is then refused as
`handoff_unavailable`. Another viewer's `Interrupt` does not stop someone else's draft. A
checkpoint whose outcome is unknown while drafting quarantines the session, as a mode change's does,
because the held copy and the disk can no longer be assumed to agree. The draft's base frontier is
taken after its own finished record, so the draft names the state it describes. A refused draft is
kept as accounting of a call that was made.

A request the adapter refuses before it is prepared (the held selection does not fit) writes nothing
and is refused with `candidate_too_large`. There is no selection identity or request digest to
record for a request that was never frozen, and a handoff is never clamped to fit. The handoff does
not search for a smaller selection, so it never hides anything on the draft's behalf.

### Commit, in order

A commit touches two sessions, and the store has no transaction across them, so the steps are
ordered to leave the more useful half behind if one fails:

1. The source is checked against the draft. It must be idle, still managed, and hold exactly the
   draft's base; otherwise the commit is refused with `stale_draft` (or `handoff_unavailable` for a
   busy, legacy or quarantined source). A name taken in the meantime is refused here too. A name is
   checked once already, before the draft's model call: one word, within the store's name rules
   (`nanus_ports::session_name_problem`), and free.
2. The new session is created with its opening records: a `context/mode` copying the source's mode
   and policy, output reserve and shell capture included, then `context/seed`. It is checkpointed
   through its own checkpoint. If this fails nothing exists and the commit is refused. An unknown
   outcome is reconciled against the disk: a session that landed is kept, one that did not is let
   go, and one whose fate cannot be read is held quarantined while the commit is refused.
3. The name, when one was asked for, is written. If this fails, the session still exists and is
   complete. The handoff is not undone: the seed's `name` says the session holds no name, which is
   how the interface tells the reader.
4. The back-link is recorded in the source as a `context/handoff` with `confirmed: true`, the
   draft's base, the new session's id and the document's digest. If this fails, the new session is
   still complete and its seed names where it came from, and the seed's `back_link_recorded` is
   false. An unknown outcome is reconciled the same way, and a source whose fate cannot be read is
   quarantined. The source's viewers are sent a `Checkpoint` frame for the back-link, except the
   committing connection, which is leaving the source for the new session.
5. The connection attaches to the new session and lets go of every draft it held of the source.
   The new session is reserved from the moment it is held until that attachment, so it cannot be
   evicted as an idle session nobody watches in between.

`back_link_recorded` is not remembered from the commit: on every attachment the agent reads the
source session's log for a `context/handoff` naming the new session, so a later attachment, by any
agent, gets the same answer. A source that cannot be read reads as `false`; the field claims only
what was seen.

The provenance the new session holds is the authority. The back-link is a convenience for a reader
of the source, which is why a failure of step 4 is reported rather than undone.

A session may hand off more than once. Each back-link names a distinct new session, and their
frontiers strictly increase: a repeated or backwards record is refused on read. Two drafts of one
session invalidate each other, because each draft's attempt records move the frontier past the
other's base. The first commit wins and the second is `stale_draft`, and the reader drafts again.

### The seed

The seed is a managed record, `context/seed`, and may appear only as the second event of a session,
directly after the managed `context/mode` record that opens it. Anywhere else, after a legacy mode
record, or in a legacy body, it is refused, on write and on read. Its text passes the text rule, and
its `document_blake3` must be the digest of that text. A back-link must name, as its source, the
session that holds it.

The managed compiler renders the seed as a leading system message, after the system prompt, on every
request the session makes, the first included (`compile::seed_message`). It opens
with a fixed label (`HANDOFF_SEED_LABEL`) saying it is a handoff a model wrote from an earlier
session, generated data rather than a user instruction and not proof, then `Source session: <id>.`,
a blank line and the document. A leading system message is the one slot every request has: the
generated message must sit after the first user message and never last, so a session's first
request, which is only the first prompt, has no place for it, and a seed carried there would be
missing from exactly the request that needs it. It is not a user message, because the model that
wrote it is not the human. It is not a fragment, so it cannot be hidden, and it is composed into
every candidate the fitter probes, so its bytes count toward the request's fit.

A seeded session that is switched to legacy context keeps its seed. The legacy prompt is assembled
from the log rather than by the managed compiler, so it places the same seed message — label and
document — directly after the system prompt, before the first user message, where the legacy fit's
leading prompt keeps it and never drops it. Otherwise the switch would silently leave the new
session with nothing it was started from. A legacy session that was compacted no longer sends the
seed: the compaction request was given it, and the summary stands in for it as it does for the rest
of the conversation before the cut. A session with no seed assembles exactly what it did.

### Compaction is legacy-only

A managed session is never compacted, and a compacted session cannot become managed. Managed
selection keeps every user message, which is exactly what a cut removes. So enabling managed context
on a session that has a `context/compact` marker is refused with `compacted_session`, from every
entry point, and a managed `context/mode` after a marker is refused by the session itself on write
and on read, so no path can produce or open such a file. A session that was managed and then
switched back to legacy may be compacted, even with the switch as its last event. Its body still
holds managed records, so the cut is saved through its `SessionCheckpoint`, never by a plain store
save, and the next checkpoint expects the file the cut left. Compaction is the legacy mode's only
way to shrink a prompt; it is described with the legacy fitter in `nanus_domain::context::compact`,
and this page only needs the rule that a session is one or the other.

## Accounting

Every managed request records a `request/attempt` intent before HTTP and a finished record when it
settles, durable by the next checkpoint: the exact selection identity, the projection revision,
the digest of the body actually dispatched, the outcome (`completed`, `failed`, `cancelled`,
`refused`, `unknown_dispatch`), the management fragment it carried, and nullable usage. Missing is
never zero; a usage report followed by a stream error survives as the failed attempt's usage;
repeated cumulative reports replace the attempt's snapshot rather than accumulate. A session's
totals (`usage_totals`, `usage_by_model`) count a handoff draft's finished attempt, under the model
its selection names, because a draft has no assistant message to carry its cost; a turn step's
attempt is not counted, because its assistant message already is. Monotonic timings
(response head, first token, decode, total) are measured with the process clock and never
derived from wall-clock timestamps.

## Errors

Refusals carry one of the stable codes `unsupported_mode`, `policy_denied`, `stale_base`,
`invalid_fragment`, `protected_fragment`, `invalid_reference`, `candidate_too_large`,
`protected_floor_too_large`, `protocol_incompatible`, `storage_capacity`,
`checkpoint_not_committed`, `checkpoint_unknown`, `cursor_expired`, `source_unavailable`,
`source_corrupt`, `capture_partial`, `capture_unavailable`, `cancelled` and `mixed_mutation_batch`,
with bounded ids and counts and never source excerpts. The handoff and compaction surface adds
`invalid_compact` (a marker or summary that breaks an invariant, or a response that asks for a tool
or stopped at the output ceiling), `compact_unavailable` (compaction asked of a managed, busy or
recorded session, of one with nothing after its marker, or with no session attached, and a draft
that was interrupted), `compacted_session` (managed context asked
of a compacted session), `handoff_unavailable` (a handoff asked of a legacy, busy or recorded
session or with none attached, an interrupted draft, a narrative or seed that breaks the text rule,
or a malformed seed or back-link) and `stale_draft` (a commit whose draft base moved, or whose draft
id is not the one held). Over the link a refusal's code travels in `Frame::Refused.code` beside a
sentence that never repeats it. A malformed tool argument is an ordinary bounded tool failure; a
failed context edit never invalidates an earlier result or retries a command.

## What is and is not verified

Verified by deterministic tests against the real runner, store, shell, link, interface and the
three adapter crates' encoders and decoders, including OpenAI's Responses wire, on macOS:

- legacy sessions send the same content, schemas, event trace and save path as before, with the
  elision notice in the tail and, on Responses, the system prompt as a leading developer message.
  Each adapter pins its ordinary body. The differences from the previous build are:
  - Anthropic: the `system` block form and the breakpoints the stock runner opts into;
  - Responses: the developer system message and, on the API plan, its breakpoint;
  - every wire: the placement of the elision tail.

  The runner compares a legacy run through both entry points;
- fragment derivation, protection, compilation, hard fitting, proposal staging and revalidation,
  the reset barrier and the boundary schema (a test checks every payload against
  `context-management.schema.json`);
- the step transaction — intent before HTTP, frozen dispatch, settled checkpoints, refused and
  uncertain commits and their reconciliation, recovery of an open turn without rerunning anything;
- mixed-batch refusal, host-policy denial with no bypass, pre-parse argument limits including a
  late name, admission's original and effective views, and the step budget charging management;
- recall's bounds, cursors, encodings and refusal to substitute; shell capture past the preview
  through the real shell and store, verified at checkpoint and found by recall;
- checkpoint identity, bounds and atomicity in the store, the archive's quotas, chunk verification
  and garbage collection, and deletion's retirement against a stale writer;
- frontier-aware attachment racing checkpoints, protocol-version refusal, and a managed turn over
  the real link and store;
- handoff drafts writing only their attempt records, with `kind: handoff` and the refusal, cancelled
  and prepare-time outcomes; the seed's position, its digest, its bounds and header on write and
  read, and its rendering as a leading system message on every request, the first included; recovery
  of a dangling draft intent on its own; the back-link chain's ordering; and the commit's refusals,
  its name and back-link outcomes (a failed back-link included) over the real store and link;
- compaction's cut rule and marker invariants on write and on read, its prompt fold, its draft,
  preview and commit over the real store, an interrupted draft over the link, a session managed once
  compacted through its checkpoint and then taking a turn, and that a compacted session cannot
  enable managed context, from `nanus run`, the link and the interface;
- each adapter's fixtures for a leading compaction summary in a legacy request and a leading seed in
  a managed one;
- the Responses managed body on the API and subscription plans, sent through the real adapter and
  runner to a local listener. The tests cover the order of the developer system parts, plain
  history with no replay and no `include`, `max_output_tokens` present and equal to the
  reservation on the API plan and absent on the subscription plan, images and image results, exact bytes and the same digest
  after a reload, argument limits including a name that arrives only at `output_item.done`, and a
  following reservation across a model switch;
- a following reservation's arithmetic, its record, and body version 6: the field's serde rules, a
  present `false` refused, the header and its refusal below version 6 on read, and a configured
  budget bounding the reservation a host's enable records;
- the three-part layout. On every adapter and wire:
  - three consecutive steps of a managed turn encode with a stable prefix;
  - the first-turn exception breaks only after the first user message;
  - a fit leaves the head unchanged when it changes no notes;
  - the tail is always the last content and is never recorded;
  - a message encodes identically whether or not it is the newest;
- Anthropic breakpoint placement, order and TTLs on managed and ordinary bodies, none without
  opt-in;
- signed replay under both fingerprint rules, and a legacy reply digest recorded by the previous
  build still matching;
- the OpenAI trailing developer tail, the Responses developer system message, both explicit
  breakpoints, and the session's `prompt_cache_key` on both plans at OpenAI's endpoints and on no
  other path;
- the legacy fitter: its elision notice is costed and carried as the tail, never as a message, on
  every wire, and the fitter's choice of turns is the same as before for the same budget;
- live, on 2026-10-28, on the `subscription` plan with `gpt-5.6-sol`: a Responses body with a
  developer system message and no `instructions` is accepted and answers as the system prompt
  directs, and three managed steps with a trailing developer tail after tool output are accepted.

Not verified, and not claimed:

- **Cache hit rates are not measured by the gates.** The `#[ignore]`d live cases in
  `nanus-bundle/tests/live_cache.rs` check them:
  - that each provider serves a managed step's prefix from cache at the stated thresholds;
  - that OpenAI accepts a trailing developer message after tool results, and a request with a
    developer system message and no `instructions`, on both plans and wires;
  - per Anthropic model, whether signed replay is accepted with an earlier step's tail removed.

  They need credentials, and a pass is evidence for that day and model only. Only the
  subscription's acceptance case has been run. Its cache-read case failed on its one run: the
  second step reported no cached tokens although the first step read the prefix an earlier run
  wrote. In a three-request probe that day, identical subscription requests were served from cache
  irregularly with or without a `prompt_cache_key`. With the session's key sent, three more runs
  of the case the same day read the previous step in one run and nothing at step 2 in two. The
  subscription's cache routing is not understood, and no claim is made about its hit rate.

- **No live provider has been sent a maintenance request.** A handoff or compaction draft is a model
  call built and read the way a managed turn's is. The tests show the request is built and its
  response is read and refused correctly; they do not show that a provider's summary is good, or
  that its cost is what the usage record says.
- **The subscription reservation is not enforced by the provider.** The tests show the reservation
  is charged and that the body omits it. The window the subscription is fitted to is the model's
  declared window, by decision, and has not been measured against the backend.
- **Only the subscription plan has been sent a managed request live.** The rest of the provider
  matrix rests on wire and reload fixtures; in particular, nothing yet proves a provider accepts the
  generated assistant message followed by an original one, or DeepSeek, z.ai and Inception a
  generated turn without `reasoning_content`.
- **No quality or cost evaluation has been run.** The held-out exact-retention and coding-task
  suite (acceptance case T34) needs paid, repeated live runs and has not been authorised, so there
  is no claim that managed context improves anything, and the default stays legacy.
- **A final short write is now seen, but not by a test.** Session, checkpoint and archive writes
  flush before they sync, so an error on the last chunk refuses the commit instead of
  acknowledging a truncated file; the review reproduced the earlier failure with a file-size
  limit, and the fix has no automated regression test, because inducing a short write needs a
  process-wide resource limit.
- **Durability is process-crash only.** Power-loss durability needs parent-directory
  synchronization (and `F_FULLFSYNC` on macOS) and is not claimed.
- **Native Windows** was checked by cross-target lint of the local and store adapters, not run.
- **Child-session handoff**, which the contract specifies only for a future subagent feature, is
  not implemented.
