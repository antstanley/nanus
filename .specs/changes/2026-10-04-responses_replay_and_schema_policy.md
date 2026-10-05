# Change: Explicit Responses function policy and stateless reasoning replay

**Status:** Proposed · **Date:** 2026-10-04 · **Owner:** Ant Stanley · **Target:** Provider-neutral session replay and OpenAI adapter

Complete the public API Responses contract without adding host workflows, prompts, skills,
plugins, installers or desktop state to Nanus. This proposal supplements the
[embedding change](2026-09-30-library_embedding_and_multimodal_results.md).

## Motivation and verified baseline

Published revision `ee1e59554b8d52a2ccc2bb3565f6b0dbc68914e2` has exact protocol preferences,
independent tool-support metadata and opt-in transport/decoder response limits. Its Responses
function encoder omits `strict`. Its accumulator handles function calls and display reasoning
but does not retain encrypted reasoning output items. The domain's `AssistantReplay` accepts
only `anthropic.messages`. Exact Responses routing therefore does not yet establish complete
stateless reasoning continuity or preservation of a caller's optional function parameters.
Downstream adoption deliberately refuses OpenAI configuration until both gaps are closed.

The current [function-calling contract](https://developers.openai.com/api/docs/guides/function-calling)
says omitted strictness allows Responses schema normalization; explicit `strict: false`
preserves best-effort schemas. The current
[reasoning contract](https://developers.openai.com/api/docs/guides/reasoning) supplies encrypted
reasoning items in stateless responses and describes replaying output items for continuation.
These sources were checked on 4 October 2026. The host must retain its selected model and
effort, rather than switch models or remove reasoning/tools to avoid these requirements.

## Affected canonical pages

| Page and heading | Delta |
| --- | --- |
| [Architecture → What each crate owns](../../docs/architecture.md#what-each-crate-owns) | Keep wire items in the provider adapter; keep bounded opaque replay and persistence in domain/runner. |
| [Features → Model providers](../../docs/features.md#model-providers) | Describe explicit function strictness and opt-in stateless Responses replay after implementation. |
| [Sessions → What a session is](../../docs/sessions.md#what-a-session-is) | Preserve original ordered provider replay items alongside neutral text/reasoning/calls. |
| [Sessions → Resuming](../../docs/sessions.md#resuming) | Revalidate replay identity, prefix and neutral response on store reload; never relabel another protocol's replay. |
| [Testing → The tests that matter most](../../docs/testing.md#the-tests-that-matter-most) | Add actual encoder/decoder, complete runner and store-reload signed replay/refusal cases. |

## Proposed canonical additions

### Architecture → What each crate owns (Add)

> The OpenAI adapter owns explicit function-schema strictness and stateless Responses item
> decoding/replay encoding. Domain owns protocol-specific bounded opaque replay validation;
> runner/store propagate and persist it. These APIs do not add host prompts, skills, Claude
> plugins, installers or desktop workflow state to the minimal library.

### Features → Model providers (Add)

> Library callers can explicitly select non-strict Responses functions to preserve their
> original optional parameters, or select strict functions with preflight compatibility checks.
> The absent policy retains the existing omitted field. Opt-in stateless API replay retains
> original completed reasoning/message/function-call items and refuses incompatible endpoints,
> malformed observations and crossed replay before HTTP; it does not use hosted conversation ids.

### Sessions → What a session is (Add)

> Version-2 assistant records can retain validated `openai.responses` replay as well as existing
> `anthropic.messages` replay. Original ordered items, encrypted content, phase and call identities
> persist beside neutral display text and calls. Existing record/log bounds and fallible writing
> remain; no image or replay is inferred into version-1 logs.

### Sessions → Resuming (Add)

> Opt-in stateless Responses continuations validate original-prefix identity, protocol and exact
> neutral text/calls before using completed stored replay. Changed/incomplete or orphaned replay
> refuses rather than being relabelled, dropped or sent with a hosted conversation reference.
> Existing writer claims and store ownership remain unchanged.

### Testing → The tests that matter most (Add)

> Responses schema/replay fixtures use actual encoder/stream transport, the real minimal runner
> and store reload to preserve optional parameters, encrypted reasoning, phase and sibling call
> outputs. Adjacent changed-prefix, identity, malformed, capacity, termination and cancellation
> cases refuse before HTTP/tools. These fixtures do not establish native Windows or exact-model
> live acceptance; existing provider/minimal/runtime-free gates remain required.

## Proposed contracts

### Explicit function-schema strictness

Add an OpenAI configuration policy with three states: provider default, explicit strict,
and explicit non-strict. Preserve the current omitted field when the policy is absent.
For explicit non-strict Responses requests, emit `strict: false` on every flat function
definition; preserve the caller's exact parameters, required array and optional properties.
For explicit strict, emit `strict: true` and reject incompatible schemas before HTTP;
never silently rewrite required fields or insert nullable alternatives. Keep Chat's existing
shape/default unchanged unless the caller explicitly selects the equivalent Chat policy.
The encoder used for estimation and dispatch must be the same implementation.
The host will select non-strict for its existing schemas and retain native argument validation.

The strict inlined subset also applies the current documented aggregate property, nesting,
enum and identifier/value-string ceilings; unsupported references/keywords refuse without
rewriting. Unicode limits count characters, independently of serialized JSON byte bounds.
See the [Structured Outputs contract](https://developers.openai.com/api/docs/guides/structured-outputs).

### Ordered stateless replay

Add an opt-in stateless Responses replay policy to the API adapter. On this policy, require
public API Responses, `store=false`, bounded response limits and a bounded explicit output
ceiling. Refuse incompatible subscription/Chat/proxy configurations before HTTP. No hosted
conversation, `previous_response_id`, OAuth, hosted tool or implicit storage is introduced.

Retain the original ordered completed reasoning, assistant-message and function-call output
items required for manual continuation, including encrypted reasoning, item ids, assistant
phase and function call ids. Preserve each item's documented replay fields; do not reconstruct
opaque content from display deltas, drop empty reasoning items, or confuse an item id with a
function call id. Emit the bounded replay envelope to the runner before Finished. A completed
response and its item/delta identities must agree; malformed, duplicate, contradictory,
unsupported hosted-tool, incomplete or failed output refuses the observation before tools run.

Extend `AssistantReplay` with the distinct `openai.responses` protocol while preserving
existing `anthropic.messages` validation. Preserve the existing envelope fields (`protocol`,
`prefix_digest`, `blocks`) and add an optional bounded `context_receipt` for Responses;
use protocol-specific validated items, not unrestricted JSON
passed through to another provider. Validate exact concatenated assistant text and exact ordered
function ids/names/argument objects against the neutral response. Assistant phase and annotations
are retained replay data, never instructions or additional executable calls.

Bind the envelope to a canonical digest of the actual selected endpoint/wire/model/effort,
instructions, tools including strictness policy, and preceding encoded input. A continuation
may extend the original prefix with corresponding outputs and later messages; it cannot
rewrite the signed prefix. Reject changed or missing replay, crossed protocols, mismatched
text/calls, reordered items/results and stale policy identities before HTTP. Follow the
existing whole-turn fitting rules; elision cannot leave signed items or their tool results
orphaned. Do not delete original durable history to make the fitted request valid.

#### Original and fitted prefix receipts

An opt-in Responses record retains a closed `context_receipt` with the original fitting
budget, dropped whole-turn/message counts, SHA-256 of the unelided source prefix and SHA-256
of the exact dispatched wire body. The outer prefix digest binds these receipts to the fixed
endpoint/protocol and actual model/effort/instructions/tool-schema/output controls. The source
prefix includes preceding original replay envelopes and ordered neutral tool observations;
its incremental digest avoids repeatedly encoding the entire durable history.

`ChatRequest` may carry an immutable source-history view that is never sent to the provider.
Admission verifies the fitted messages against the source, complete oldest-turn boundaries
and the exact deterministic fitting notice. Validate every historical replay against its
original source prefix and controls before using any retained original item. The dispatched
body is independently measured/hashed and includes only the admitted fitted projection.
Fitting can change the current notice and omit older complete turns without rewriting the
receipt that produced an earlier response. No original durable message is removed.

Prospective tool-result estimation may replace only the ordered result values of the most
recent assistant batch; preceding messages, calls, ids, user text and replay stay immutable.
The actual source digest captures the values selected for dispatch. These receipts establish
consistency of locally observed data, not cryptographic authentication of provider ciphertext
or arbitrary rewritten logs. Domain shape checks alone do not establish request admission.
Old Anthropic envelopes retain their existing shape with the new field absent. Stock callers
that omit history and Responses replay policy retain their existing request encoding.

Count encrypted strings, full replay items, neutral display/call copies and the complete
request/checkpoint JSON framing. Apply opt-in response byte/item/tool-slot budgets before
buffer extension or cloning. A transport or capacity refusal must not yield partial executable
calls, invent successful usage/completion, or save an unfinished turn.

### Boundary schema delta

The existing bounded replay envelope gains one protocol value; there is no desktop schema
or skill manifest in Nanus. Its conceptual JSON boundary is:

```json
{
  "$defs": {
    "AssistantReplay": {
      "type": "object",
      "additionalProperties": false,
      "required": [
        "protocol",
        "prefix_digest",
        "blocks"
      ],
      "properties": {
        "protocol": {
          "enum": [
            "anthropic.messages",
            "openai.responses"
          ]
        },
        "prefix_digest": {
          "type": "string",
          "pattern": "^[0-9a-fA-F]{64}$"
        },
        "blocks": {
          "type": "array",
          "minItems": 1,
          "maxItems": 256,
          "items": {
            "type": "object"
          }
        },
        "context_receipt": {
          "anyOf": [
            {
              "$ref": "#/$defs/ReplayContext"
            },
            {
              "type": "null"
            }
          ]
        }
      }
    },
    "ReplayContext": {
      "type": "object",
      "additionalProperties": false,
      "required": [
        "budget",
        "dropped_turns",
        "dropped_messages",
        "source_digest",
        "wire_digest"
      ],
      "properties": {
        "budget": {
          "type": "integer",
          "minimum": 1,
          "maximum": 4294967295
        },
        "dropped_turns": {
          "type": "integer",
          "minimum": 0,
          "maximum": 4294967295
        },
        "dropped_messages": {
          "type": "integer",
          "minimum": 0,
          "maximum": 4294967295
        },
        "source_digest": {
          "type": "string",
          "pattern": "^[0-9a-fA-F]{64}$"
        },
        "wire_digest": {
          "type": "string",
          "pattern": "^[0-9a-fA-F]{64}$"
        }
      }
    }
  }
}
```

Encrypted content remains opaque; Nanus cannot decrypt or independently authenticate provider ciphertext.
Preservation checks compare original observed bytes and bound replay receipts; provider validation
remains authoritative for cryptographic validity. Do not claim local ciphertext verification.

The existing 4 MiB complete serialized Nanus record bound and protocol-specific closed item validation
remain mandatory beyond this shape. Hosts may impose stricter bounds, such as a 1 MiB complete event-line cap. Actual deserialization, estimation and replay admission
must enforce these constraints; this fragment alone does not establish their implementation.

## Implementation pointers and acceptance

### Local preparation status — 5 October 2026

An isolated workspace based on public ee1 implements explicit function policy, closed Responses
validation/v2 reload, the bounded original-item decoder and a pure request preparer.
`OpenAiLlm::prepare_responses` obtains exact model/endpoint capabilities from the actual adapter,
checks complete immutable source history, validates previous source/control receipts, admits only
an exact whole-turn projection and emits original ordered items with `store:false`, explicit
output ceiling and `include:["reasoning.encrypted_content"]`. It returns the exact body's SHA-256,
original source SHA-256 and fitting counts for `StreamAccumulator::with_context`.

`ChatRequest::source_history` is an optional shared immutable source; it is never encoded into HTTP.
The minimal runner retains it when provider-aware fitting is requested. Historical images receive
individual media/profile checks before fitting; aggregate image caps apply to each actual candidate.
An opaque-only completed Responses message remains in the source fold even without display text.
Absent receipts, stock encoders, existing Anthropic replay and default decoder behavior remain valid.

Actual downstream minimal-runner tests compose the pure preparer and decoder, execute sibling
calls, reload v2 sessions and admit the next continuation. Exact whole-turn elision is covered by
request fixtures without deleting original source. These are local fixtures, not paid API evidence.
The preparer performs no HTTP and does not authenticate ciphertext or rewritten receipts.

`OpenAiConfig::set_stateless_responses(true)` now explicitly selects the admitted body and
receipt-aware decoder in the actual transport. Construction requires the exact public Responses
endpoint, no subscription account and explicit response limits. Model/tool metadata and budgets
are checked per request. This mode disables redirects; a second origin is never silently selected.
The default remains false, so existing automatic/Chat/subscription/gateway paths retain behavior.

Opt-in `LlmPort::estimate_request` uses `estimate_responses`: it may substitute only values in the
last complete ordered assistant/tool batch, while calls/ids/users/prompts/prior observations and
whole-turn fitting remain exact. Estimates dispatch nothing and expose no admission receipt.
Actual `stream_chat` uses the strict preparer and refuses those substituted candidates. A host
rebuilds immutable source from real retained results before commit/dispatch.

Actual checked preparation → HTTP POST/SSE → minimal runner → complete batch admission and
sibling results → v2 reload → lower-budget whole-turn fitting is covered by localhost fixtures.
They redirect only the private transport's socket argument; production still derives its public
URL from checked configuration. Truncated completion executes no pending calls; opaque-only
completed output survives reload and is replayed after the next human turn. Redirect refusal
is observed with a second listening origin receiving no connection.

This candidate remains unpublished. A downstream dependency pin still targets public ee1 and
refuses OpenAI readiness; immutable publication/adoption, host original-record/batch authority
and live/native acceptance remain required. Stock composition does not automatically opt in or
access new credentials.

### Supported item-schema evidence

The supported subset was checked against OpenAI's generated SDK revision
[`becc1d20eed83c1b8d85e15dc131a372d9dc7813`](https://github.com/openai/openai-python/tree/becc1d20eed83c1b8d85e15dc131a372d9dc7813/src/openai/types/responses).
The [`ResponseReasoningItem`](https://raw.githubusercontent.com/openai/openai-python/becc1d20eed83c1b8d85e15dc131a372d9dc7813/src/openai/types/responses/response_reasoning_item.py)
schema explicitly distinguishes incomplete added ciphertext from completed done ciphertext.
The [`ResponseOutputMessage`](https://raw.githubusercontent.com/openai/openai-python/becc1d20eed83c1b8d85e15dc131a372d9dc7813/src/openai/types/responses/response_output_message.py)
schema records assistant phase, and
[`ResponseOutputText`](https://raw.githubusercontent.com/openai/openai-python/becc1d20eed83c1b8d85e15dc131a372d9dc7813/src/openai/types/responses/response_output_text.py)
defines the four retained annotation variants. Original file SHA-256 receipts were kept locally
outside the repository.

This preparation accepts completed ordinary function calls, assistant output/refusal text,
empty reasoning with nonempty opaque ciphertext and the documented annotation metadata.
It deliberately refuses hosted output items, nonempty log-probability payloads and unknown
item fields. These are explicit subset boundaries, not claims of supporting every Responses
feature or of local cryptographic validation. Exact-model live acceptance remains separate.

1. `nanus-adapter-openai/src/config.rs`, `responses.rs` and `lib.rs`: explicit policy,
   identical estimation/dispatch, ordered bounded item decoding and refusal before HTTP.
2. `nanus-domain/src/message.rs` and `session.rs`: distinct replay validation, exact neutral
   agreement, original-item persistence and reload, balanced complete histories.
3. `nanus-ports` and `nanus-bundle`: existing replay event/observation propagation; preserve
   record-admission and complete-batch ordering, cancellation and stock defaults.
4. Actual fixture transport: two sibling function calls with optional arguments, signed empty
   reasoning and assistant phase; original output linkage through multiple steps and reload.
   Verify exact API path, store=false, strict=false, model/effort/ceiling and full body estimates.
5. Negative adjacent cases: changed prefix/policy/model/effort/endpoint, malformed encrypted data or changed original replay evidence,
   orphan/duplicate/reordered output, wrong ids, unsupported items, incomplete terminal, failed
   response, over-budget SSE/tool/replay/error buffers and cancellation before completion.
6. Existing Anthropic signed replay, DeepSeek reasoning, z.ai Chat, automatic routing and
   runtime-free/minimal embedding gates remain green. Native Windows and exact-model live
   follow-ups remain explicit acceptance; fixture success cannot promote them.

### Host integration findings and additional acceptance — 5 October 2026

The isolated embedding host of sealed Nanus
`eeba343848e614c7cd74dbb19c35062f5c87aff4` now constructs explicit non-strict stateless
Responses and proves actual decoder → app worker → completed checkpoint → same-prompt resume
with original reasoning/message items. These fictional streams establish local continuity only.
Production app startup remains unavailable before credential access. Native tool effects and
changing trusted instructions are additional requirements, not consequences of adapter admission.

1. **Trusted instruction revisions.** A host's ordinary prompt may legitimately change between
   completed turns. The host reproduces rejection after publishing a brief that selects another
   prompt module: `source_prefixes` compares old receipts against the current System message and
   controls. Add an explicit host-owned revision mechanism that retains each historical instruction
   version and validates each original receipt against the version actually dispatched. Keep the
   current instructions in the new request. Version the durable evidence as needed; reject missing,
   crossed, reordered or model-invented revision evidence. Changes to prior user/tool/assistant
   records, model/effort/endpoint/schema/strictness controls still refuse. Do not recompute stored
   receipts over altered history, freeze old instructions, silently start a fresh session or discard
   history. Nanus receives ordinary text and opaque host revision identity, never brief/skill/plugin
   concepts. The exact versioned representation must be reviewed before implementation; the existing
   `ReplayContext` schema above is unchanged until that representation is specified and implemented.
2. **Pure prospective estimates.** Return checked complete costs for valid bounded prospective
   candidates even when the cost exceeds the selected request budget. Such estimates convey no
   permission or dispatch receipt. Actual preparation/transport still rejects oversized requests.
   Preserve original-source and complete final-batch substitution checks. The host's native image batch
   currently reaches the real estimator but its worst bounded failure alternative does not fit:
   39,524 estimated input tokens before the alternative plus the 8,192-token output reservation
   leaves insufficient room for the complete escaped failure. Separating measurement from dispatch
   is necessary for consistent caller budgeting. The later consumer trace below corrects the
   initial aggregate-capacity conclusion: the host reserves durable/audit bounds independently, then
   admits actual model delivery. Require actual image/text tool execution under the original
   limits; no lowered result allowance or silent context-budget increase counts as a fix.
   Also repair and test Responses image accounting: the sealed shared estimator subtracts encoded
   pixels only under `messages`, while Responses uses `input`/`input_image`. Its current request
   cost therefore includes base64 as text in addition to the visual profile. A consumer envelope
   that adds only visual-token growth and wire-byte growth cannot assume that subtraction occurred.
   Require actual varied-size PNG/JPEG payloads to prove complete estimated token/wire growth, and
   preserve full cost for lookalike image fields inside tool arguments or schemas.
3. **Real multimodal input.** The current analysis composer uses a synthetic assistant function call
   and tool result to carry images. Strict Responses correctly refuses that unobserved assistant
   replay. Add a bounded generic multimodal request-message seam, with exact provider encoding,
   image/record/request estimation and history/persistence semantics, before admitting this route.
   Never manufacture provider output items or replay receipts. This does not move video workflows,
   dependency installation, skill discovery or plugin behavior into Nanus.

Required regressions include unchanged-prompt continuation, an authorized instruction revision
through completed-history reload and whole-turn fitting, every adjacent unauthorized mutation,
complete prospective cost beyond the budget with refused dispatch, native consumer tool execution,
and actual multimodal input with rejected oversized/unsupported images. Malformed/truncated
Responses streams must execute no pending calls. Immutable publication/adoption and original live,
Windows and packaged acceptance remain separate gates.

**Scoped semi-formal review.** Premises: caller prompts may change, historical provider items stay
original, and capacity must be admitted before effects. Resolution: the host's turn snapshot selects
ordinary prompt modules; Nanus `responses/request.rs::controls` and `source_prefixes` bind current
instructions to every old receipt; `prepare_mode` invokes `validate_estimate` even for prospective
measurement. Trace: completed original response → new prompt → refusal before HTTP; original
image/text calls → worst failure cost → refusal before tool-action audit. Same-prompt completed
replay succeeds. Verdict: **PARTIAL** integration; the guard prevents premature readiness, while
these three generic seams and their consumer proofs remain implementation work. No protocol/source
checks are relaxed, and no additional public boundary is claimed implemented by this prose.

### Local cost correction — 5 October 2026

A child of the sealed reconciliation candidate corrects Responses image accounting and prospective
measurement. The shared estimator subtracts inline `input_image` payload strings only from actual
Responses user-message content, while preserving full wire bytes and visual-profile charges.
Image-shaped schemas/calls/text remain charged. The actual original-item preparer and PNG/JPEG
fixtures verify the formula. Pure estimates can report an over-budget valid candidate; actual
preparation/transport still require the unchanged fit check. Source/control/replay and complete
final-batch substitution validation remain mandatory. No public boundary type changes.

All fourteen scoped gates pass with 251 frozen inputs unchanged; see the
[cost certificate](2026-10-04-responses_replay_and_schema_policy.review.md#responses-image-cost-and-prospective-measurement-certificate--5-october-2026).
The sealed source is `bd5ac14ba17c63dc330c6abe1462a9204ba843ad`. Its actual embedding host
passes six Responses coordinator fixtures: native image/text execution, original-pixel replay
through completed checkpoint reload, oversized-read delivery refusal retaining complete audit,
same-prompt replay, malformed/truncated refusal and credential-free production readiness refusal.
The consumer does not need maximum-success request capacity before effects: complete durable/audit
capacity is reserved first, then actual delivery and final commit each check the unchanged model
budget. The earlier inference that pure estimates alone could not admit the native batch was wrong.
Trusted prompt revisions, genuine multimodal analysis input, immutable adoption and original
acceptance remain required. The host's full verification is recorded in its own change review.

### Trusted instruction revision representation — 5 October 2026

The next local implementation uses an explicit default-off adapter setting,
`OpenAiConfig::set_instruction_revisions(bool)`, requiring the existing exact public stateless
Responses mode. Host composition owns the leading `Message::System` values. A new instruction
version is permitted only at a new user-message boundary, never between calls/results in one
model turn. Nanus receives ordinary text; it gains no brief, skill, plugin or installer API.

`ReplayContext` gains optional `instructions: ReplayInstructions`; omission retains the exact
legacy receipt serialization and prefix semantics. The closed snapshot has `version: 1`, an
opaque SHA-256 `revision` of its ordered text array, and `messages: Vec<String>`. At most 64
System texts and 256 KiB of serialized snapshot data are permitted. Empty lists are valid;
empty or duplicate System text is preserved exactly. The preparer counts borrowed data before
cloning, including JSON escaping. The snapshot and complete context also count against the
selected decoder event limit and the existing replay/record/session envelopes.

Revision mode uses a separately tagged source digest that excludes only the current leading
System messages. Every user/assistant/tool message, including complete older replay contexts,
remains in the ordered incremental hash. Each original response is checked against its stored
instruction snapshot and the current unchanged endpoint/model/effort/schema/strictness/output/
temperature controls. The adapter checks each snapshot revision against its texts, retains all
stored digests unchanged, and sends the current System texts in the next wire request. Historical
instruction changes within one user turn refuse; a changed current prompt without a new user
boundary also refuses. A text-array digest identifies a revision; no app revision id is needed.

Legacy and revision-aware histories cannot be mixed or relabelled. Default mode keeps accepting
legacy unchanged-prompt receipts and refuses revision-mode receipts. Revision mode refuses old
receipts with no historical snapshot, even if the prompt happens to match; hosts must preserve
those histories and use an explicitly new conversation to opt in. No snapshot or receipt is
synthesized for an old response. Body version 2 still carries bounded opaque replay; the nested
version makes old-reader rejection explicit without rewriting existing files.

The actual request preparer creates the snapshot before provider contact. The decoder receives it
through `PreparedResponses`, never from model output. Pure prospective measurement retains the
same source/revision checks and grants no dispatch receipt. Whole-turn fitting keeps original
history for validation, and the discarded-turn counts exclude the current leading System header.
This is consistency validation, not authentication of externally rewritten files or ciphertext.

Acceptance covers default compatibility, authorized changes through decoder and completed-session
reload, whole-turn elision, different System-header counts, repeated/empty texts, unchanged old
provider items, mid-turn changes, legacy/mode mismatch, every adjacent nonprompt/control/snapshot
mutation, escaped byte/count limits, and bounded decode when the snapshot consumes replay space.
The host's actual changed-brief test must pass before removing that part of its readiness guard.

The representation above is locally implemented and passes all fourteen scoped gates, with
255 unchanged frozen inputs; the [implementation certificate](2026-10-04-responses_replay_and_schema_policy.review.md#instruction-revision-implementation-certificate--5-october-2026)
records exact evidence and limitations. Actual downstream brief-change adoption remains required.

The sealed instruction candidate is `dcd7c75ae00b729fb21f74b925866dd3cd84f4f8`.
The host's six real coordinator fixtures pass against it. Publishing a changed brief changes the
next System prompt while preserving original items; completed checkpoint reload resumes the
same session and new prompt. Native image/read replay and oversized-result refusal still pass.
The production app guard now names genuine multimodal analysis. The host's full verification is
recorded in its own change review; public pins and original acceptance remain unchanged.

## Assumptions / Decisions / Open questions

**Assumptions:** the published immutable baseline above is authoritative, independently of
an owner's dirty checkout or earlier local prepared branches.

**Decisions:** make function strictness and stateless replay explicit adapter policies.
Preserve stock omitted strictness and text paths until callers opt in. Keep generic persistence
and ordering in the minimal harness; hosts retain their own source/key/payment/setup authority.

**Open questions:** review and implement generic multimodal request input. Trusted instruction
revisions and the real downstream brief-change/reload fixture are locally verified. Pure prospective cost measurement and native consumer image/read
delivery under its unchanged capacity contract are locally verified; production readiness remains guarded. Original-prefix/fitting and opt-in transport are
locally implemented in the sealed candidate above, but immutable adoption and exact-model live
acceptance remain unrun. The supported item-schema evidence is pinned above.
