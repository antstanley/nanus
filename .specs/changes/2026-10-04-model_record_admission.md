# Change: Optional admission before user and model records

**Status:** Proposed · **Date:** 2026-10-04 · **Owner:** Ant Stanley · **Target:** Generic embedding ports and runner lifecycle

Add opt-in host callbacks before turn/user records, each model request and its assembled
assistant/call records enter the session. Hosts reserve complete durable records and
validate observations before tool admission. Nanus retains one provider-independent loop
and has no knowledge of skills, plugins, prompt templates, installers or desktop workflows.

## Motivation

The local complete tool-batch seam runs after the assistant and copied call audits have
entered memory. It cannot prevent an earlier durable-capacity failure or reserve before
provider contact. A bounded decoded response may expand beyond a host's complete JSONL
line cap through escaping, signed replay or copied call audits. Checking only on save
can discover this after a native effect.

The embedding host has pure turn/opening/closing counters, but the runner provides no callback
before TurnStart/UserMessage or StepStart. Those counters alone are not production admission.
This proposal fills that generic lifecycle gap; complete tool/result authority remains in
the existing batch seam and physical ownership remains with the caller.

## Affected spec pages

| Canonical contract | Change |
| --- | --- |
| [Design → A budget, because unattended loops are a cost hazard](../../docs/design.md#a-budget-because-unattended-loops-are-a-cost-hazard) | Add optional host record/request admission, preserving stock bounds. |
| [Sessions → What a session is](../../docs/sessions.md#what-a-session-is) | Keep exact append order and independent caller persistence; explain refusal before append. |
| [Safety → Defaults, and what they do not protect you from](../../SAFETY.md#defaults-and-what-they-do-not-protect-you-from) | Capacity is separate from source, process, key and payment permission. |
| [Testing](../../docs/testing.md) | Record actual-runner ordering, failure and cancellation evidence once implemented. |
| [Optional complete tool-batch admission](2026-10-03-tool_batch_admission.md) | Compose reservations without counting an already retained prefix twice. |

No JSON/wire entity or session-format change is proposed. New interfaces are local Rust ports.

## Proposed changes

### Design → A budget, because unattended loops are a cost hazard (Add)

> Optional record admission checks the original retained history and exact literal user
> text before opening a turn. Each step reserves complete possible durable model records
> and closing paths before model contact. The assembled observation is validated before
> its assistant/call records are appended or a tool policy, approval or executor is called.
> Default absence preserves the existing stock loop and bounds.

Add `with_record_admission` and a caller-owned local synchronous port. Exact names may
follow established port conventions; the required phases and borrowed data are below.
Callbacks perform no provider, source, decoder, process or secret I/O. No mutable session,
model handle, key or fabricated original receipt is exposed.

| Phase | Borrowed original projection | Required behavior |
| --- | --- | --- |
| Reserve turn, before TurnStart/UserMessage | Session identity, actual durable events, next sequence/turn and literal user text | Return an owned turn reservation for complete start/user records and all terminal paths. Refusal leaves the log unchanged and sends no model request. |
| Reserve step, before StepStart/model contact | Actual prefix, exact turn/step and next sequence, held selection epoch/model/effective effort, capabilities, original unelided and fitted requests, held pure estimator | Return an owned step reservation for start, full possible assistant, every copied call audit and closing paths. Unknown/unfittable bounds refuse before HTTP. |
| Validate assembled observation, before append | Exact selected identity and original text/reasoning/replay/calls/usage/interrupted state, with the same fields used by actual record construction | Check complete escaped framing and every exact ordered audit copy before record allocation/copy and before any tool policy/approval/dispatch. No truncation, invented usage or replacement successful observation. |
| Validate closing records, before append | Actual proposed StepEnd/TurnEnd and any other runner-owned closing records with their original position and sequence | Confirm they fit the already reserved failure/cancellation/success path, without new source or provider effects. |

The projection must borrow the actual session identity/records and original selected
adapter's estimator. A flattened request cannot replace the actual durable log. A numerical
selection epoch cannot prove a caller's provider/endpoint/protocol/account/key lifetime.
The caller captures and holds those identities independently. Centralize actual record
construction and counting so the projection cannot diverge from the records appended.
Bound raw argument JSON before ToolCall's custom JSON-string serializer allocates it;
bound complete replay/assistant records before concatenation or duplicate audit allocation.

Reuse the existing held-selection mechanism, extending its activation to either admission
port. Hold the adapter/model/effort through request construction, stream consumption,
observation validation, tool-batch execution, ordered append and StepEnd. Preserve its
bounded queued setters and non-wrapping epochs. Do not make record admission depend on
registered tools: final answers and request/stream errors must participate too.

### Sessions → What a session is (Modify)

> Record admission does not save the session. Hosts retain original header/file/action
> authority and acknowledge completion only after their persistence port succeeds.
> An admitted turn preserves exact user text and ordinary event order. An observation
> refused before append contributes no assistant or executable call audit; the runner
> closes the admitted step/turn through its already reserved bounded failure path.

A turn reservation stays owned through its TurnEnd and releases on error, cancellation
or dropped future. A step reservation stays owned through StepEnd and all tool-batch
callbacks. Dropping logical capacity must not free a still-running physical worker or
its source/process lease. The consumer's checkpoint acknowledgement remains independent.
Before turn admission, cancellation/refusal must not append a synthetic turn. Once admitted,
all ordinary exits must use reserved closing records; an observation refusal cannot proceed
to tools, a second model request or a successful completed-history acknowledgement.

Use a fixed bounded, secret-free runner diagnostic for admission failure. Never append
arbitrary host/vendor diagnostics to an exhausted log. Do not pretend earlier completed
effects were rolled back. If original persistence/closing authority itself is lost, return
an explicit failure for host recovery; do not fabricate a balanced durable checkpoint.

### Complete tool-batch admission → Runner integration (Modify)

Share one consumer ledger across original turn, step and complete-batch reservations.
A tool projection already includes the accepted assistant/call prefix; the batch callback
must count it as retained, not add the opening allowance a second time. Transfer or settle
future capacity exactly once while retaining the same original step/selection receipt.
No newly constructed event vector, numeric header length or equal call values may substitute
for the original pending state, audit baseline, header identity or held caller authority.
Internal goal tools remain covered by complete-batch admission and closing reservations.

### Managed context coordination (Add)

From [bounded, recoverable context management](../../docs/context-management.md); a coordination
note for when this candidate is adopted, not a claim that its API is shipped:

> Managed turns reserve exact context, artifact, attempt-intent, attempt-outcome and
> recovery-closing records alongside original conversation records. The selection hold activates
> for either admission port or managed context. Request preparation precedes the durable intent
> checkpoint; all original record authority remains with the caller. Synchronous admission performs
> no saving. Hosts use the independent checkpoint receipt and persistence state to decide whether
> any further save or dispatch is permitted.

As implemented, `StepRecordProjection.fitted_request` is the managed effective request, and
`ToolBatchProjection.managed` carries the accepted revision, the effective request and its pure
estimator beside the unchanged original projection.

## Implementation notes

1. Add local Rust contracts in `nanus-ports`; export them without stock dependencies.
2. Modify `AgentRunner::run_controlled` before its turn/user append and `run_step` before
   StepStart/model contact and assistant/call append. Use owned guards for every early exit.
3. Reuse held selection in `agent_loop/selection.rs`; integrate closing/dispatch helpers
   without duplicating the loop or moving caller policy into Nanus.
4. Keep exact event construction and borrowed counting aligned. Use actual-runner fixtures,
   not a test-only reimplementation of session/dispatch behavior.
5. Reconcile the unpublished prerequisite branch with the owner's newer public main in an
   isolated checkout before implementation/adoption. Do not rewrite or publish owner work.

## Acceptance criteria

- Default absence yields the existing event/request/tool traces and stock gates unchanged.
- Turn refusal preserves byte-for-byte history, literal input and all caller capacities;
  no StepStart, request, policy, approval or executor runs.
- Step refusal and failed request construction make no HTTP request and close only an
  already admitted turn through reserved actual records. Final-answer steps participate.
- Exact line/file/event ceilings, escaped/Unicode text, signed replay and full call arrays
  fit below/at their limits and refuse the next byte/record before copied allocation/effects.
- Crossed model/effort/epoch/position or reordered/changed call copies refuse before tools.
- Model failure, early EOF, cancellation at every phase, queued selection changes,
  overlapping held steps and dropped futures release logical capacity once and preserve
  separate physical cleanup ownership.
- Complete-batch projection reuses retained model records without double counting; goal,
  denial, mixed result and multi-step/multi-turn paths retain original order and usage.
- Immediate invalid usage still cancels independently. Capacity checks never qualify
  usage validity, image support, account authority or payment permission.
- Credential-free minimal domain/ports/runner, embedding consumers, formatting and
  all-target/all-feature warning-denying Clippy pass. Native Windows execution, stock
  credential-aware gates, live providers and host adoption remain separately recorded.

## Semi-formal proposal review

**Premises.** Current local ToolAdmission reserves after assistant/call retention.
`run_controlled` appends turn/user immediately; `run_step` appends StepStart before request
construction and appends the assembled assistant before running tools. No record port exists.
The public DeepSeek revision supplies no such hook. Existing stock behavior must remain
unchanged when the new port is absent.

**Resolution and trace.** Literal input → turn reservation → original start/user append;
held selection and pure request construction → step reservation → StepStart → provider stream
→ bounded complete observation validation → original assistant/call append → existing batch
reservation/dispatch/ordered results → actual reserved StepEnd/TurnEnd. Rejecting before
turn admission leaves no new records; later rejection uses reserved failure closing only.
Host persistence follows independently, and no failed observation reaches a native executor.

**Findings resolved in the design.** Reserving at the tool callback is too late for model
records; new callbacks precede those appends. A step-only callback misses user and terminal
records; turn/closing phases are explicit. Per-call counters can double-count retained copies;
composition uses one original shared ledger. Releasing a logical guard cannot establish quiet
physical teardown. Replay and custom ToolCall encoding require bounded counting before joins
and copied allocations. No smaller model/line/file limit is introduced to make a reservation fit.

**Verdict.** LIKELY_CORRECT as a proposed generic lifecycle contract. No implementation,
compilation, tested host wiring or publication is claimed. Actual callback ordering, exceptional
exits and capacity handoff still require the listed real-runner acceptance fixtures.

## Assumptions and open questions

**Assumptions**

- An embedding host supplies honest complete envelopes and original header/key/source/audit
  authority; the generic harness cannot infer those from sizes or copied values.

**Decisions**

- _Optional local mechanism._ **A synchronous admission port and owned reservations.** Keep
  stock composition unchanged and desktop workflow out of Nanus.
- _Publication._ **Proposal only; no push or dependency adoption.** The owner has not approved
  publishing the prepared prerequisite branch, and newer public main needs reconciliation.

**Open questions**

- Exact Rust trait names and shared ledger handoff should be finalized against actual-runner
  tests; the phase order, failure behavior and independent authority requirements are fixed.

## Isolated implementation evidence — 4 October 2026

This owner checkout still treats the API as proposed: its current source has no
`RecordAdmission` implementation. A separate isolated candidate jj workspace merges
public `d03f8958bd0144f938a4c3330954a6886bddeee4` and prepared
`4d24df7d093f3d1912d3047eef33c61e51e81242` without rewriting either parent or owner source.
Its local bookmark `codex/record-admission` points at
`efee35247a1c3fc1b101f03efb71b3049cddc0b3`, including documentation-only updates after
verified inputs `ee5ed890cb4d90ad3538a23c3d99c5eb7e4b2b6a`.

The isolated implementation certificate is preserved in commit `efee352` (tag
`archive/record-admission-efee352`). It records actual port ordering, owned logical leases,
original observation moves, replay-join ordering fixes and nineteen new actual-runner cases. Final scoped minimal tests pass 443
including four doctests; admission/embedding passes 50; provider/video passes 217; embedded
default/providers pass 26/29; runtime-free TUI passes 342. Suites overlap. No case fails or
is ignored. Workspace warning-denying Clippy, formatting and Windows MSVC-target library
Clippy pass. All 297 tracked inputs stayed unchanged during the final gates. Windows
cross-compilation is not native execution. Logs were kept locally outside the repository.

No publication or downstream dependency adoption occurred. The embedding host's original
authority/ledger consumer and live/native platform acceptance remain unbuilt. The prepared
publication bookmark remains at `4d24df7d`; its pending approval does not authorize publishing
this new candidate. No paid model, OS secret store or personal Chrome was used.

## Reconciled record and Responses candidate — 5 October 2026

A new isolated child of `dc9cb6104e5aea9c2544b9d164af4d6b1e2e9a4e` selectively reconciles
this port with current public-ee1 source-history fitting, explicit Responses transport and
prospective final-result estimation. It preserves the owner checkout, older record candidate
and sealed Responses parent. The [combined patch certificate](2026-10-05-record_responses_reconciliation.review.md)
resolves the actual functions and records two reproduced fixes: canceled turns cannot return
old-turn answers, and opt-in host counting precedes runner-side Responses replay parsing.

Fourteen scoped gates pass with 249 frozen inputs unchanged: affected tests 539, minimal
runner 183, all-feature admission/embedding 53, embedded default/providers 29/35, runtime-
free TUI 342, two minimal doctests, formatting, warning-denying Clippy and Windows-target
minimal-library Clippy. Five actual embedding-host capacity-consumer tests also pass separately.
Counts overlap; no native Windows, credential-aware stock, paid/live API or complete host
production authority is inferred. The local evidence was kept outside the repository.
This owner's proposal remains Proposed pending publication and actual production adoption.
