# Change: Optional complete tool-batch admission

**Status:** Implemented locally (unpublished; verification scoped) · **Date:** 2026-10-03 · **Owner:** Ant Stanley · **Target:** Generic embedding ports and runner lifecycle

Expose the pending tool batch and read-only next-request projection before tool effects.
An embedding host supplies complete result/failure envelopes, conservative capacity
reservations and authority checks. Nanus orchestrates these callbacks without knowing
about desktop workflows, skills, plugins, installations, video briefs or credentials.

## Motivation

`ToolPolicy` approves an exact call but cannot read retained history or other pending
calls. `ImageEnvelope` reserves image count/file bytes, but cannot prove complete wire,
text/schema/framing/token/context capacity. Checking the next request after execution
can discover exhaustion after source disclosure, decoding or paid analysis already
occurred. The optional video proposal already requires this generic missing seam.

## Affected spec pages

| Canonical contract                                                              | Change                                                                                           |
| ------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| [Design](../../docs/design.md#only-three-fields-of-a-tool-can-reach-the-model)  | Keep projection and callbacks local; separate capacity and permission.                           |
| [Safety](../../SAFETY.md#defaults-and-what-they-do-not-protect-you-from)        | Document physical ownership and host authority limitations.                                      |
| [Testing](../../docs/testing.md)                                                | Record credential-free lifecycle evidence and remaining gates.                                   |
| [Video proposal](2026-10-01-read_video_extension.md#aggregate-result-admission) | Supply its generic pending-batch/lifecycle prerequisite, without claiming a host implementation. |

Rust contracts are canonical; no wire field, JSON Schema or session-version change.

## Proposed changes

### Ports → Immutable batch projection and reservation (Add)

`ToolAdmission::reserve(&ToolBatchProjection) -> Result<Box<dyn ToolBatchReservation>, AdmissionError>`
is synchronous, opt-in, and must perform no tool effects or network/secret I/O.
The borrowed projection contains session/turn/step, stable model-selection epoch,
the system/schema/history/calls request, every pending call and any already refused
outcomes, exact selected capabilities and a read-only pure request-estimation callback.
An immutable `events` slice includes actual durable records omitted by message replay;
it contains no prospective failures. Hosts reserve complete checkpoint header/framing
and future records independently of request fitting. It exposes no mutable session,
adapter handle or credential. The host captures its
provider/plan/account/endpoint/protocol identity alongside its own authority epochs.
The request is unelided; a separate `fitted_base` uses existing whole-turn fitting.
A private prospective session copy pairs every pending call with an actual denial or
fixed failure slot, including signed replay. Fabricated slots never enter durable history.
The host replaces slots with conservative complete text/image/framing envelopes in
call order and estimates them using the same held adapter.
Unknown envelopes or a base that cannot fit refuse the batch before dispatch.

The owned reservation has synchronous `admit`, `before_dispatch`, `validate_result`
and `commit` operations. `admit` runs once per permitted call before any batch effects;
its refusal produces a bounded image-free result without invoking the executor.
`before_dispatch` rechecks live caller authority immediately before every goal or
registered executor; sticky cancellation is checked again after this callback.
`validate_result(call, raw, retained)` sees both the original untrusted observation
and the globally bounded/media-validated result before progress completion or retention.
Private raw JSON can differ from the retained model value; hosts use bounded counters
to check their actual audit envelope rather than trusting normalized model content. A refusal replaces all
content with a fixed bounded image-free failure whose allowance the host reserved.
`commit` sees the assembled, fitted next request after all results were appended in
call order; failure closes the step before provider HTTP. Its returned error closes
admission; it never claims that effects are rolled back.

The reservation stays owned until ordered append and commit finish; dropping it on
cancellation/error/future drop releases caller capacity through its own Drop policy.
It does not release physical process/readers: caller workers retain their own leases.
Already-denied outcomes are visible to reserve; the host retires their unused policy
receipts. `release_unreserved` retires unused local handles when projection/reservation
fails or the future drops during approval; successful reserve transfers cleanup to
the owned lease. Every goal and registered tool participates, not just an image producer.

### Runner → Optional integration and stable selection (Modify)

`with_tool_admission(Rc<dyn ToolAdmission>)` installs the port. Default absence keeps
existing stock composition, policy, image admission and selection semantics unchanged.
With the port installed, hold adapter/model/effort selection from request construction
through stream consumption, approval, dispatch, result validation and commit. Queue
setter changes in three bounded latest-value slots; apply them after the last active
step hold releases. Selection epochs advance without wrapping; an exhausted epoch
refuses optional admission. No mutable borrow crosses an await. Concurrent steps on
one runner retain the same held selection until both commit or release.

On reserve failure, settle unanswered calls with fixed bounded failures, preserve
prior denials and return a context failure. Close StepEnd and TurnEnd using existing
error paths. On cancellation, commit the ordered interruption outcomes when a lease
exists, retaining capacity until append. No suppressed failure or fabricated success.

## Implementation notes

1. Add local ports in `nanus-ports/src/tool_admission.rs`; export public Rust contracts.
2. Add runner-local selection hold and admission orchestration modules. Refactor the
   long existing tool runner into bounded gate, dispatch and ordered-append helpers.
3. Add external actual-runner fixtures for before-effect refusal, projection contents,
   cross-chunk reservation lifetime, failure/replacement, cancellation/future drop,
   queued model/adapter/effort changes, ordered commit and default absence.
4. Run credential-free ports/domain/minimal runner tests, all-feature admission-only
   tests, whole-workspace warning-denying Clippy, fmt, scoped doctests and standalone
   embedded modes. Leave real credential-aware stock gates unrun, explicitly stated.

## Acceptance criteria

- Host reservation sees complete immutable batch/current request before any executor.
- Refused calls never execute; validation precedes success progress and ordered append.
- Capacity stays retained across all chunks until complete ordered commit or Drop.
- Selection setters cannot change the admitted adapter/model/effort during a held step.
- Default absence preserves existing minimal/stock behavior and model wire allowlists.
- No callback provides OS containment, source permissions or physical-worker completion.

## Semi-formal proposal review

**Premises.** Existing per-call permissions and declared image admission remain separate.
The host must project complete envelopes across the pending batch before effects; a
static executor cannot derive this from ToolCall alone. Late validation must precede
completion and retain reservations until ordered append. Provider selection must remain
the one used by the request/projection, even across awaits and cancellation.

**Resolution/trace.** Gate every call; retain existing image denials; build a fitted
read-only projection; reserve the whole batch; admit calls in order; recheck before each
dispatch; globally validate then apply host result validation; append all observations
in call order; fit/commit the complete next request; release reservation then held
selection. An impossible base settles failures and closes context before effects.
Drop releases logical capacity while caller physical leases remain independent.

**Fixed design findings.** Keep denial outcomes in the projection; hold selection from
before the first model await; support overlapping steps with a hold count; bound pending
changes by field rather than an unbounded queue; reject epoch exhaustion; use fixed
refusals with reserved failure slots; close StepEnd on new context errors. The generic
seam alone does not establish that a particular host actually implements its budgets.

**Function resolution.** `run_step` holds Selection before request/model await. Its
`run_tools` creates the unreserved guard, records/gates calls, keeps image refusals,
and invokes `admission::reserve_batch`. Projection construction clones Session,
`append_results` adds prospective slots, and ordinary `derive_messages` now includes
all paired calls/replay. `dispatch::execute_permitted` checks every goal/registry
executor with the same owned lease. `finish_admitted` passes original raw outcome
and `bounded_result`/media-normalized model outcome to the host, before ToolFinished.
Ordered actual append and `build_request` precede lease commit. Selection Drop applies
queued choices only after the final active hold; unreserved/lease Drop handles cleanup.
No mutable adapter, credentials, session or registry borrow crosses an await.

**Fixed implementation findings.** Ordinary replay drops unanswered calls, so using
it without prospective result slots omitted the whole pending batch. The private-copy
projection fixes that. Normalization removes private values, so result validation
now receives raw and normalized observations; the actual-manifest regression proves
they differ. Sticky cancellation raised inside before_dispatch now prevents execution.
Provider messages omit durable call/step/goal records, so the projection now also
exposes the actual event slice for full checkpoint admission. The complete-history
fixture confirms old records remain and fabricated results are absent.
Initial helper/fixture lint and render-newline expectations were corrected without
suppression. Failed runs and final logs were kept locally, outside the repository.

**Regression/evidence.** Final credential-free domain/ports/minimal runner nextest
passes 419 tests, no skips. Fifteen new actual-runner cases and one epoch unit cover
all chunks, refusal/denial, complete old history, signed/pending pairing, raw manifest,
goal effects, cancellation, commit/context error, approval/tool future drop, selection
queue and overlapping steps. All-feature admission passes 24 tests; four scoped
doctests, embedded default/providers 7/10 tests, whole-workspace/minimal all-target
warning-denying Clippy and Windows-target domain/ports/minimal library Clippy pass.
Runtime-free TUI 342 tests and Clippy passed at the preceding admission checkpoint;
final workspace Clippy compiles its unchanged consumers against the raw-result API.
Formatting passes. No stock credential-aware workspace tests/doctests were run: these
can read real Keychain even with a changed NANUS_HOME. No personal browser, paid
provider, native Windows execution or upstream publication is used or inferred.

**Verdict.** LIKELY_CORRECT for the scoped local generic lifecycle. The consumer must
still implement real complete request/token/audit/checkpoint envelopes, provider and
authority epochs, failure slots and physical-worker ownership. Logical Drop and
result refusal cannot stand for process join or rollback. The consumer's published pin has
not adopted this work; original video/provider/native acceptance remains open.

## Merge plan

Canonical design/safety/testing/features/status pages and the spec index now record
the scoped local implementation. Keep the broader video proposal partial until its host, provider and native gates pass.
Do not publish the consumer's repository or change its immutable pin implicitly.

## Assumptions and open questions

**Assumptions**

- Embedding callbacks obey the pure synchronous contract and reserve failure allowances.
- Host endpoint/account identity and worker lifetimes are captured by its own ports.

**Decisions**

- _Minimal harness._ **Opt-in generic ports and owned Drop lifetime.** No video-specific
  orchestration or desktop authority enters the runner.
- _Selection._ **Queue adapter/model/effort while a held step is active.** Keep stable
  disclosure identity and apply later choices for the next request.

**Open questions**

- None for this seam; the consumer budget implementation and immutable adoption remain
  explicit follow-on work, as do original provider/native/package acceptance.
