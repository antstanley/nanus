# Semi-formal review: Responses policy and replay proposal

**Date:** 2026-10-04 · **Scope:** the [proposed generic change](2026-10-04-responses_replay_and_schema_policy.md), against public Nanus ee1 and the downstream consumer's revision under review. This file retains the initial specification review and subsequent scoped patch certificates; none establishes live-model acceptance.

## Premises

P1. Public ee1 supplies exact routing/tool metadata/response budgets but omits Responses strictness and retains only display reasoning. Existing AssistantReplay validates Anthropic Messages exclusively.

P2. The downstream consumer retains optional native tool parameters, manually persists completed sessions and requires encrypted reasoning and call/output continuity. It cannot compensate by dropping tools or changing the selected model.

P3. Nanus remains a minimal provider-neutral harness. App prompts, installers, source authority and paid-analysis decisions remain outside it. Existing stock wire defaults and Anthropic replay must remain valid.

## Function resolution and required traces

Actual public `responses::encode_tools` emits type/name/description/parameters without strictness. `OpenAiLlm::encode` and checked estimation select that encoder via the actual exact protocol. Actual `responses::StreamAccumulator` emits display ReasoningDelta and function calls; its item-added/done handlers consume only function calls. `AssistantReplay::validate` accepts only `anthropic.messages`. No existing replay path establishes encrypted Responses continuity.

Proposed path: explicit non-strict API policy → same encoder for estimate and dispatch → strict=false with unchanged optional parameters. Completed bounded original Responses items → protocol-specific replay validation → exact neutral text/calls plus original opaque ordered items → completed log/store reload → unchanged signed prefix with corresponding outputs → manual stateless continuation. A malformed/incomplete/over-budget or crossed replay refuses before tool execution or next HTTP.

## Findings and corrections

1. **Cryptographic overclaim:** a negative case initially said altered encrypted content must be rejected locally. Nanus cannot authenticate arbitrary provider ciphertext. The proposal now distinguishes bounded shape and original-observation preservation from cryptographic validity, which remains the provider's responsibility. The negative case requires malformed content or changed original replay evidence, not an invented decryption capability.
2. **Unnecessary baseline narrowing:** the illustrative digest pattern initially required lowercase hex, while existing Anthropic validation accepts both cases. The fragment now preserves that existing shape. Generated digests may remain lowercase; no unrelated existing Anthropic acceptance is removed.
3. **Default and ownership checks:** explicit policy is opt-in, preserving omitted stock strictness. Replay retains phase/ids and does not turn annotations into calls. Provider items do not introduce skills/plugins, desktop concepts, hosted tools or stored-conversation authority.

## Regression and sufficiency

The proposal requires actual encoder/stream/runner/store-reload cases, including sibling calls, encrypted empty reasoning, optional arguments and phase; local constructor tests alone cannot certify them. Response and complete-record budgets apply before cloning. Historical fitting cannot orphan signed prefixes. Existing Anthropic, DeepSeek, z.ai, automatic routing and minimal/runtime-free gates remain required. Native Windows and live exact-model checks remain separate.

All local Markdown targets in the new proposal/review resolve; the linked canonical headings exist. No Nanus Rust source, dependency pin, production branch or published API is changed by these proposal files. No runtime gate or live API success is claimed here.

## Verdict

**CORRECT as a proposed missing-capability contract; high confidence.** The review findings are fixed in the proposal. Implementation, immutable publication/adoption and the listed acceptance evidence remain required.

## R1 canonical consistency certificate

### Step 1 — Premises

P1: Targets docs/architecture.md → What each crate owns; docs/features.md → Model providers; docs/sessions.md → What a session is and Resuming; docs/testing.md → The tests that matter most.
P2: Add opt-in explicit Responses function policy and bounded stateless original-item replay across adapter, domain, runner and store.
P3: Preserve stock omitted strictness, existing provider routing, Anthropic replay, version-1 compatibility rules, writer claims and minimal host boundaries.

### Step 2 — Reference resolution

- Architecture ownership Add: (1) `../../docs/architecture.md` exists; (2) named heading exists; (3) no Modify/Remove claim; (4) the section has existing crate ownership/minimal embedding but no explicit Responses schema/replay ownership claim; (5) RESOLVED.
- Features provider Add: (1) `../../docs/features.md` exists; (2) named heading exists; (3) no Modify/Remove claim; (4) exact preference/tool/response metadata exists but explicit function policy and stateless Responses item retention do not; (5) RESOLVED.
- Sessions log Add: (1) `../../docs/sessions.md` exists; (2) What a session is exists; (3) no Modify/Remove claim; (4) version 2 and signed Messages exist, but Responses replay does not; (5) RESOLVED.
- Sessions resume Add: (1) same file exists; (2) Resuming exists; (3) no Modify/Remove claim; (4) writer-claim ownership exists, but opt-in Responses original-prefix replay admission does not; (5) RESOLVED.
- Testing Add: (1) `../../docs/testing.md` exists; (2) named heading exists; (3) no Modify/Remove claim; (4) real tools/socket/multimodal tests exist, but the complete schema/encrypted reasoning/phase/reload proof is absent; (5) RESOLVED.

Coverage: each affected row has a corresponding named canonical Add block; no unlisted canonical target. Supplementary contracts expand those blocks rather than modifying an unnamed page.

### Step 3 — Consistency trace

- Architecture ownership Add: canonical separates pure domain/ports, adapters and minimal runner; the change places wire policy/items in the adapter and persistence validation in domain/runner/store. CONSISTENT.
- Features provider Add: canonical preserves automatic selection and optional bounds; the change keeps stock defaults and introduces opt-in strictness/replay only. CONSISTENT.
- Sessions log Add: canonical version 2 stores typed content/signed Messages with 4 MiB records; the change adds a second validated replay protocol and retains version-1 rules and existing bounds. CONSISTENT.
- Sessions resume Add: canonical writer claim guards ordinary continuations; the change adds replay validation without replacing locks or ownership. CONSISTENT.
- Testing Add: canonical requires actual tools/socket evidence; the change requires actual decoder/runner/store proofs and preserves separate live/platform acceptance. CONSISTENT.

INTERNAL: envelope fields, protocol discriminator, opt-in defaults, bounds and five Add blocks agree with the contracts and acceptance list.

### Step 4 — Schema check

- $ref resolution: the AssistantReplay fragment has no $refs; all RESOLVED.
- Modified entity base: protocol/prefix_digest/blocks and complete closed envelope match current domain serialization; the new protocol extends the existing envelope. Existing hex-case acceptance and actual 4 MiB record bound are preserved. MATCHES.
- Prose/schema parity: AssistantReplay is named in contracts and the $defs fragment; no new desktop entity is introduced. MATCHED. Protocol-specific item validation and existing full serialization bounds explicitly supplement the illustrative shape.

### Step 5 — Edge cases and fixed findings

The review found missing named canonical additions and a loose root schema fragment; explicit per-target Add blocks and an AssistantReplay $def now resolve both. It also caught an incorrect 1 MiB claim for Nanus's existing record limit: the actual limit is 4 MiB; a host's stricter 1 MiB line limit is now stated separately. Opaque cryptographic validity and hex-case compatibility were corrected above. Supported item-schema evidence and original/live acceptance remain explicit implementation requirements.

This pass read every named target section end to end, not every unrelated section of the four whole canonical pages. The verdict reflects that context limit; it does not certify repo-wide canonical accuracy.

### Step 6 — Verdict

VERDICT: LIKELY_CONSISTENT
CONFIDENCE: high
SUMMARY: All named targets, additions, schema parity and preservation constraints resolve after the recorded fixes; unrelated whole-page context was not re-reviewed.
SUGGESTIONS:
- Before implementation/merge, pin the supported Responses item schema and complete the proposed encoder/stream/runner/store acceptance, then repeat R1 with full canonical-page context.

## Local decoder/policy patch certificate — 4 October 2026

### Premises

P1: The isolated ee1-based candidate changes OpenAI configuration/function encoders,
adds a separately constructed replay decoder and extends domain validation for Responses.
It does not enable replay in stock `stream_chat`, publish a revision or change a downstream
dependency pin.

P2: The implemented scope must preserve exact optional parameters, original completed opaque
items and neutral text/calls; reject invalid/incomplete/over-budget observations before tools;
retain domain/runner replay through v2 serialization/reload.

P3: Stock omitted strictness and default stream behavior, existing Anthropic replay,
DeepSeek/z.ai paths, v1 compatibility and minimal host ownership must remain valid.

### Function resolution

- `OpenAiLlm::checked_protocol` calls private `function_policy::validate` before estimate/dispatch;
  its structural walk and `function_policy::limits::Counts` enforce the selected strict subset.
  The actual Responses/Chat builders call `function_policy::apply` on flat/nested functions.
  These are crate-local implementations, not provider transformations or external validators.
- `responses::StreamAccumulator::with_prefix` constructs `responses::replay::Replay` with
  immutable bounds. `observe_frame` checks frame/call limits, then Replay lifecycle/delta
  agreement, then existing neutral event production. The ordinary default has no Replay.
- Replay done-item validation resolves to domain `AssistantReplay::validate_item` and the
  closed Responses helper. `capacity::check` borrows the prospective ordered envelope and
  counts its complete serialization before the item clone. It does not decrypt ciphertext.
- `StreamAccumulator::close` derives exact neutral calls, then `Replay::finish` calls domain
  `validate_response` before queuing replay/calls/usage/Finished. Failure clears pending state.
- Actual HTTP `response::decode`/`Decoder::Responses` tests use this constructed decoder.
  Downstream fixtures feed it into the actual `AgentRunner::consume_stream`, which resolves
  the same domain validation before tool execution. The session writer/reader retain v2 items.

### Execution traces

Before: function schema → omitted strict; reasoning added/done → discarded original ciphertext;
only neutral text/calls reach the runner.

After, explicit false: original schema → the same encoder used for estimate/dispatch →
strict=false, optional fields and required array unchanged.

After, caller-admitted decoder prefix: response identity → bounded indexed added items →
matching deltas → closed completed original items → identical ordered terminal output →
exact neutral agreement → replay before executable calls → runner/tool results → v2 reload.

Changed item/call ids, phase, raw arguments, text, order, ciphertext shape, unsupported items,
missing terminal, failed/incomplete response or exceeded capacity → Error with no queued
executable call, usage or successful completion. This proof grants no request-prefix admission.

### Findings fixed

1. Completed phase and usage were insufficiently checked. Initial/final non-null phase must
   agree; non-null errors/incomplete details refuse; usage counters must be bounded, consistent
   and partition-compatible. Missing usage remains missing rather than fabricated.
2. A final-only replay size check could clone over-limit original items. Borrowed prospective
   envelope serialization now includes the digest/protocol wrapper and JSON escaping before
   each done-item clone; exact-limit and next-byte fixtures cover this admission.
3. Local traversal limits exceeded documented strict provider ceilings. The strict subset now
   also bounds aggregate properties, nesting, enums and Unicode name/value characters under
   the current Structured Outputs contract; incompatible enum types/duplicates refuse.
4. Refusal delta support initially changed the default decoder. It is now gated on explicit
   replay construction; an adjacent default-decoder fixture preserves the prior stock behavior.
5. The first downstream positive fixture left the fictional executor at its default access,
   so the real policy denied its calls. It now declares Read; production approval is unchanged.

### Regression and remaining edge cases

The preparation preserves absent policies/default decoders and the existing envelope shape.
Existing Anthropic/DeepSeek/z.ai/store tests and the minimal runner/TUI gates remain required.
Supported SDK item schemas are pinned in the change spec. The implemented subset intentionally
refuses hosted items, nonempty logprobs and unknown original-item fields.

The supplied digest is trusted caller input, not verified by this primitive. Stock transport
does not enable it. Missing request replay/prefix admission and whole-turn fitting prevent a
complete stateless API-continuation verdict. A full-history digest alone cannot survive valid
oldest-turn elision with a changed notice; bounded original/fitted prefix evidence must resolve
that requirement before enabling dispatch. Blanket elision refusal is not completion.

### Verdict

**LIKELY_CORRECT for the local decoder/policy/domain preparation; high confidence.**
The whole proposed stateless continuation remains **PARTIAL**, with request-side admission,
encoding, fitting and immutable publication/adoption still required. This certificate does
not certify live exact-model acceptance, native Windows or credential-aware stock composition.

### Verification receipts

Final affected-provider/domain/ports/store verification passes **519 tests**, no failures.
The six explicit policy, six domain replay, seven decoder, two HTTP/SSE and two downstream
runner/reload cases exercise the new contracts (23 new cases in total). Minimal-runner
nextest passes 159 and TUI nextest 342; standalone embedded default/providers pass 7/12.
Workspace all-target/all-feature Clippy, minimal/TUI/downstream Clippy, formatting and
minimal runner doctests pass. The runtime-source manifest has 224 frozen inputs and no
drift through the final affected gates. Earlier minimal/TUI/default checks consume unchanged
inputs: their final changes are confined to the optional OpenAI adapter and its tests.

The first local test invocation failed because sandboxed localhost bind was denied; the
fictional fixture rerun passed. Intermediate lint findings and one ambiguous fixture integer
were corrected; no lint suppression or weakened production approval was added. Logs and
source/file receipts were kept locally outside the repository.

Windows MSVC-target domain/ports/minimal-library Clippy also passes; this is compilation,
not native Windows execution or OpenAI transport acceptance. Review of 100 new-file functions
finds a maximum of 63 lines and no function over 70. All 85 local links in the six affected
spec/canonical documents resolve, including the certificate anchor.

## Original/fitted request patch certificate — 4 October 2026

### Premises

P1: This extension changes domain replay receipts/source folding, context projection, request
metadata, minimal-runner provider-aware fitting and the OpenAI pure original-item preparer.
P2: Original durable history must survive whole-turn elision and bind prior Responses records;
changed prompt/model/effort/tool policy, rewritten prefixes and incomplete batches must refuse.
P3: Existing stock encoders/dispatch, Anthropic replay and absent new fields retain their behavior.
This certificate covers pure request preparation, not stock transport adoption or paid acceptance.

### Function resolution

- `AgentRunner::build_request` calls `validate_history_image_input`, then `fit_with_source` with
  each candidate measured by the actual `LlmPort::estimate_request`; the shared `source_history`
  contains the unelided messages. Candidate image validation still enforces the eight-image cap.
- `context::identify_projection` resolves to the domain's closed projection helper. It compares
  the retained leading prompt and suffix, checks a complete human-turn cut and the exact notice
  (including budget/counts); it does not grant authority based on a notice string alone.
- `OpenAiLlm::prepare_responses` obtains `capabilities` and `tool_call_support` from the actual
  adapter. Its private preparer checks the exact public Responses endpoint, explicit limits,
  complete original history and previous receipts before `encode_replay_input` copies items.
- `source_prefixes` hashes each serialized source message incrementally; every prior receipt
  matches that exact prefix and current controls. Ordered pending ids and globally unique call
  ids establish complete neutral batches. Original SHA, fitted-body SHA and fitting counts are
  bound into the outer replay digest. These hashes prove consistency, not external authenticity.
- `encode_replay_input` uses the existing image/result group encoder and appends original
  reasoning/message/function items verbatim. The raw stock `encode_input` continues to reconstruct
  neutral assistant items. The prepared body requests encrypted reasoning explicitly.
- `StreamAccumulator::with_context` checks the closed receipt shape; borrowed prospective
  envelope capacity includes the optional receipt before each original-item clone.
- `SessionLog::derive_messages` and `Message::is_empty` retain opaque-only Responses replay.
  Serialization is optional/closed; absent fields preserve existing envelope JSON.

### Execution traces

Before: the concrete adapter validates a nine-image original history against an eight-image
request cap before the fitter can remove a complete old turn, so legitimate fitting refuses.
After: every original image is individually valid → exact-model candidate estimates → old turn
omitted whole → eight-image candidate admitted → all nine originals remain durable/reloadable.

Prepared request → exact source/control/body receipts → bounded real completion decoder →
original encrypted reasoning, phase and two ordered calls → real minimal-runner tools/results →
v2 session reload → prior receipts checked against original source → original items and balanced
outputs in next request. The downstream fixture uses fictional frames and never dispatches HTTP.

Whole-turn fit → original source retained in Arc → complete old turn omitted with exact notice →
previous replay validated against its original source → current body contains only retained turns.
Changed source/control/receipt, partial/reordered batch or invented notice → refusal before encoding.

### Findings fixed

1. The new body initially omitted `include:["reasoning.encrypted_content"]`. The pure preparer now
   requests the ciphertext needed for stateless continuation; the exact-body fixture checks it.
2. Aggregate history image validation happened before fitting. Split individual original validation
   from candidate aggregate limits; the regression fake now overrides estimation like the concrete
   adapter, so reinstating the old full-history probe makes the positive case fail.
3. The surface fold discarded opaque-only Responses output. It now retains completed replay even
   without visible text/calls; original observation and v2 round-trip fixtures cover this case.
4. Variable-sized controls were cloned before their byte check. A borrowed serialized controls
   view is bounded first. Non-finite or negative request temperatures refuse explicitly.
5. Optional receipt storage enlarged a session event enough to fail the enum-size lint. Box only
   the internal optional receipt; its closed JSON shape and existing absence remain unchanged.

### Regression and edge cases

The preparer is a separate pure API. Stock `stream_chat` and default estimates keep existing
behavior. Old records without a Responses context receipt remain readable but cannot be admitted
by this stricter preparer. Existing Messages replay retains its own protocol rules and refuses a
Responses receipt. Images/results retain their existing group ordering; no skills/plugins, stock
secrets, hosted conversation references or app concepts enter Nanus.

Prospective final-batch result substitution and actual transport opt-in remain unimplemented.
The exact-source preparer must not be used as that prospective estimator until this is resolved.
No local hash authenticates provider ciphertext or an attacker who rewrites all source/receipts.
Native Windows runtime, live exact-model acceptance and immutable publication/adoption are open.

### Verdict

**LIKELY_CORRECT for the pure preparation, source/fitting and receipt extension; high confidence.**
The full stateless transport integration remains **PARTIAL**. Verification receipts below describe
this extension separately from the preceding sealed decoder candidate.

### Extension verification receipts

All 13 final scoped gates pass: workspace formatting and all-target/all-feature Clippy;
529 affected domain/ports/provider/store tests; minimal runner nextest (161), Clippy and
doctests; embedded default/providers (7/13), Clippy and formatting; runtime-free TUI nextest
(342) and Clippy; Windows MSVC-target domain/ports/minimal-library Clippy. No drift exists
in 231 frozen runtime/config/test inputs. Review of 153 new-file functions finds a maximum
of 59 lines and none over 70. All 86 scoped local links resolve and the JSON fragment
parses. Logs and source/count receipts were kept locally outside the repository.

Intermediate new fixtures had constructor/closure type mistakes and initially omitted required
added/delta observations; these were corrected to exercise the actual complete decoder lifecycle.
The new literal-model fake received the lint-required static lifetime. No production lint
suppression, weakened approval or claimed live acceptance was added. Credential-aware stock
composition was not run; workspace Clippy compiles it without invoking its secret store.

## Explicit transport/prospective-estimation certificate — 5 October 2026

### Premises

P1: This extension adds a default-false OpenAI config opt-in, dispatch preparation/transport
selection and pure final-batch value estimation. Domain projection shares its strict cut/notice
logic with a narrowly defined result comparison. The preceding immutable-source API remains.
P2: Actual HTTP must send the exact admitted original-item body and decode using its captured
receipt. Pure estimates may substitute final complete-batch values but authorize no dispatch.
P3: Default automatic/Chat/subscription/gateway behavior, existing providers and stock composition
remain unchanged. The downstream consumer's published pin/readiness are not changed by this
local preparation.

### Function resolution

- `OpenAiConfig::set_stateless_responses` sets only the default-false flag. `OpenAiLlm::new`
  checks vendor, fixed public endpoint, account absence, exact protocol and response limits before
  construction; only this mode uses the no-redirect reqwest policy. No secret store is invoked.
- Actual `LlmPort::estimate_request` selects inherent `estimate_responses` only under the flag.
  Its call to `responses::estimate_request` resolves to the private pure estimator re-export,
  not the port method. `prepare_mode(prospective=true)` validates original source/controls and
  uses domain `identify_tool_result_projection` for the final ordered complete batch only.
- Strict `prepare_responses` uses `prepare_mode(false)` and exact domain comparison. Actual
  `stream_chat` calls `prepare_dispatch`, which selects strict preparation and constructs
  `with_context` before `transmit`; the prospective estimator is never dispatch admission.
- The shared domain helper derives an eligible final tail from the actual assistant call list
  and one corresponding ordered Tool per call, with nothing after it. Its comparator changes
  only result values/errors/blocks; all earlier messages, assistant arguments, ids, lengths,
  cuts, retained suffix and exact fitting notice remain checked. Incomplete/older tails get no
  exemption. Adapter source-prefix checks still validate every old record, including elided ones.
- `transmit` compact-serializes that same prepared Value into the POST body and owns the already
  admitted decoder. ResponseHead precedes the common bounded HTTP/SSE decoder. Stock paths use
  their previous encoder/default decoder; the refactor adds no retry or credential lookup.
- The socket fixture changes only the private `transmit` endpoint argument after actual public-
  configuration preparation. Production derives `url(protocol)` internally. FixtureModel forwards
  actual adapter estimation and checked preparation into that same transport/decoder and runner.
- `AgentConfig::with_context_budget` sets the tested fitting budget; its constructor's fourth
  argument is the system prompt byte ceiling. The fixture uses the actual named budget method.

### Execution traces

Pure pending batch → immutable failure-slot source → substitute only two final result values →
actual adapter estimator accepts/costs → strict prepare/stream refuses the candidate → original
source remains unchanged. Reordered ids, altered assistant, partial batch, changed notice/budget
or older result before a later human question → refusal.

Actual admitted request → captured POST (`store:false`, explicit output ceiling, encrypted
reasoning include, optional schemas with strict=false) → fragmented SSE → original replay before
calls → real ToolAdmission prospective estimation → two runner executors → exact result commit →
v2 reload → reduced context budget → complete oldest turn omitted with notice → original durable
source retained and next actual HTTP request has only the new turn. This is fictional local I/O.

Truncated SSE → no completed replay/call release → runner error → zero tool effects, no successful
replay in the folded/reloaded log. Opaque-only completed reasoning → stored replay without display
text → v2 reload → next request contains that original encrypted item before the next user.

HTTP 302 → bounded status error → no executable call/replay/success → second listening origin sees
no connection. Default-false callers retain their existing routing/client/default decoder.

### Findings fixed

1. Generic reqwest redirect following could undermine the newly bound endpoint receipt. Opt-in
   clients disable redirects; a real two-origin socket test observes HTTP 302 and no second contact.
2. macOS accepted sockets inherited the fixture listener's nonblocking mode. Explicitly reset each
   socket to blocking with bounded read/write timeouts; this preserves reliable failure evidence.
3. An initial fixture passed its desired context budget as a prompt byte ceiling. Function resolution
   caught the mismatch; the named context-budget setter now establishes real whole-turn fitting.
4. Inherited workspace default features could not be overridden by the new test dependency. Use
   a direct dev-only minimal-bundle path, leaving the production graph and stock defaults intact.

### Regression, evidence and verdict

Thirteen final scoped gates pass: 537 affected provider/domain/ports/store tests; minimal runner
nextest 161, Clippy/doctests; embedded default/providers 7/13, Clippy/formatting; runtime-free TUI
nextest 342 and Clippy; workspace formatting/all-target/all-feature Clippy; Windows MSVC-target
minimal-library Clippy. All 232 frozen runtime/config/test inputs remain unchanged. Eight new
cases cover four actual socket/runner paths, two estimator/config and two projection paths.
188 new-file functions are at most 59 lines. Evidence was kept locally outside the repository.

**LIKELY_CORRECT for explicit local transport and prospective estimation; high confidence.**
Publication/adoption, host record/batch authority, native Windows execution and exact-model live
acceptance remain open. Credential-aware stock composition was not run. This certificate grants
no ciphertext authentication and no downstream production readiness or full migration completion.

## Original-record reconciliation — 5 October 2026

The [combined certificate](2026-10-05-record_responses_reconciliation.review.md) covers a new
isolated child of sealed `dc9cb610`, retaining current Responses source/receipt/transport
definitions while adding original-record admission. Actual stock and hosted HTTP/runner/
reload/fitting fixtures pass. It fixes cross-turn cancellation answers and defers runner-side
Responses argument parsing until after host record admission. Neither the sealed parent nor
production downstream pins are rewritten. Publication/adoption and original acceptance remain open.

## Responses image-cost and prospective-measurement certificate — 5 October 2026

**Premises.** The consumer needs exact assembled wire bytes and conservative input charges,
with encoded images replaced by profile costs. Pure estimation must expose an oversized valid
candidate's complete cost; only actual preparation/transport grants dispatch admission. Source,
replay, function policy, output reservation and complete final-batch projection remain unchanged.

**Resolution.** `capabilities::estimate_payload` calls `encoded_image_bytes`, now also traversing
Responses user message `input` content. `responses_image_bytes` accepts only the emitted inline
`input_image` position; function items, assistant messages, arbitrary schemas and JSON inside text
retain their complete byte charge. The same `visual_cost` still validates/counts retained pixels.
`responses::request::prepare_mode` always validates shape, source history, original receipts and
final-batch projection. It skips only the final `validate_estimate` for the private prospective
branch; the public estimator returns a cost, never `PreparedResponses`. Preparation and actual
transport retain the fit check.

**Traces.** Actual valid PNG/JPEG -> exact body -> wire-byte charge including base64 -> input
charge excluding encoded image strings plus the same visual profile. Different compressible/noisy
files with identical dimensions change wire bytes but not visual input charges. Image-shaped fields
in tools, calls, call outputs, assistant messages and text retain all growth. Original completed
Responses items and paired pixel results pass the actual encoder/preparer. An oversized final
result produces a conservative estimate; strict preparation and stream dispatch still refuse.

**Regression and edges.** The image regression failed on the parent (684 versus 222 tokens for
its smallest fixture); the prospective regression failed because measurement prematurely applied
context admission. Both pass after the fix. Original-source substitutions outside the last complete
batch and incompatible replay/controls remain rejected by the existing suite. The first broad pass
found test-only unnecessary qualifications and clones; these were corrected without suppressions.
The first sandboxed focused run could not bind twelve loopback listeners; the authorized fixture
rerun passed without any real credential or provider endpoint.

**Verdict: CORRECT for the scoped cost/measurement correction; integration remains PARTIAL.**
The consumer's actual native result envelope, trusted prompt revisions and real multimodal analysis
input remain separate consumer work. No app budget/result bound is changed, no effect authority
is inferred from a number, and no live provider or native Windows execution is established.

**Verification.** All 14 scoped gates pass, with 251 runtime/config/test inputs unchanged:
affected domain/ports/provider/store tests, minimal runner, all-feature admission/embedding,
minimal/downstream/runtime-free TUI lint and tests, doctests, formatting and Windows-target minimal
library lint. Exact per-suite counts are in `/private/tmp/nanus-responses-cost/verification.json`;
logs and the two red regressions are retained beside it. The suites overlap. Forty-eight functions
in the changed files are within 70 lines (maximum 59). Credential-aware stock runtime tests and
paid acceptance remain unrun. This candidate is unpublished and does not change the public pin.
