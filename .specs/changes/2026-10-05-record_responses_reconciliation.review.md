# Semi-formal review: Original record admission with stateless Responses

**Date:** 2026-10-05 · **Scope:** isolated `hype-responses` candidate, child of sealed
`dc9cb6104e5aea9c2544b9d164af4d6b1e2e9a4e`, itself based on public
`ee1e59554b8d52a2ccc2bb3565f6b0dbc68914e2`. No publication or production adoption.

This certificate reviews the reconciliation of the
[record-admission proposal](2026-10-04-model_record_admission.md) with the
[Responses transport contract](2026-10-04-responses_replay_and_schema_policy.md).
The older `efee3524` candidate and sealed Responses parent remain unchanged.
Only the record port, runner changes and their fixtures were reconciled; unrelated old
DeepSeek, documentation and owner-source changes were not copied.

## Premises

P1. Turn/user and possible model records need original before-append reservations; a
complete-batch callback after assistant retention cannot establish that earlier capacity.

P2. The Responses parent supplies source-history fitting, opaque replay receipts, explicit
function policy, strict transport preparation and prospective final-result estimates.
Reconciliation must preserve those actual definitions, stock default behavior and independent
caller authority. Nanus receives no skill/plugin, installer or desktop workflow concepts.

P3. A turn canceled before its first model contact has no answer of its own. Raw records,
usage, earlier completed effects and the previous turn's answer remain retained in history.

## Function resolution

- `nanus_ports::RecordAdmission` and reservation traits resolve to the new pure local
  `record_admission` module, not stock composition, secret ports or copied Hype counters.
- `AgentRunner::run_controlled` calls `records::reserve_turn_records` before original
  TurnStart/UserMessage; its local `drive_turn` closes through `end_turn_records`.
- `run_step` calls the single `selection::hold_selection`, active for either port.
  `records::begin_step_records` calls the current `build_request`; its unmodified source-history
  Arc and whole-turn fitting are the Responses parent's definitions, not the old candidate's.
  Its estimator captures the held `LlmHandle` and calls that adapter's `estimate_request`.
- `perform_step` calls the one extracted `stream::consume_stream`; no duplicate original
  method remains. Assembly still uses the original `Assembled::absorb/settle/interrupt`.
  With record admission, `absorb_event` moves replay without runner-side shape parsing;
  without it, original stream-time validation remains in place.
- `records::append_model_records` moves assembled fields into the exact AssistantMessage,
  invokes `StepRecordReservation::validate_model`, then calls the actual domain
  `AssistantReplay::validate_response`. That method invokes `validate`, which invokes
  Responses `function`/`arguments` and neutral-response checks. Only then does the runner
  clone executable calls, append the event and enter the unchanged `run_tools` batch path.
- `last_assistant_text` is the runner-local helper. Its reverse scan now stops at the current
  TurnStart, preserving the latest nonempty current-turn prose without crossing turn identity.
- The HTTP fixture's `FixtureModel` resolves to real `OpenAiLlm::prepare_dispatch/transmit`
  and the actual bounded decoder, changing only the private fixture socket URL. The record
  and batch fixtures implement real ports; they do not reimplement the runner.
- Hype's isolated consumer uses actual `studio_agent::checkpoint::TurnCapacity`, StepCapacity,
  and RecordLedger through real callbacks. Fictional model/tools and permissive fixture checks
  do not establish original production key, source, request, audit, payment or worker authority.

## Execution traces

Admitted success: literal caller input → owned turn reservation → original turn/user append
→ held selection and current request fitting → owned step reservation → StepStart → strict
Responses body/HTTP/SSE → original assembled observation → host record validation → domain
replay validation → original assistant/call prefix → complete-batch reservation → sibling
executors → exact ordered results/commit → batch drop → reserved StepEnd → step drop →
reserved TurnEnd → turn drop → independent host persistence/reload.

Refused observation: completed bounded provider observation → host refuses original record
→ no assistant/call/result retention, batch callback or executor → reserved failure StepEnd
and TurnEnd → owned leases drop. Provider work already completed is not undone.

Resumed success: v2 reload preserves opaque items and source history → new user/turn admission
→ lower context budget removes only the old whole turn from wire history → old literal input
and observations remain durable → new bounded response is admitted and closed.

Cancellation regression: previous completed answer → new admitted turn → step callback
cancels before contact → interrupted closing → reverse answer scan stops at new TurnStart
→ empty answer; previous answer stays in durable history. The stock pre-contact cancellation
path has the same current-turn answer boundary.

## Findings fixed and sufficiency

1. **Cross-turn answer leakage.** An actual two-turn/reload test failed because the reverse
   helper returned old prose when the new turn had no observation. Restricting that scan to
   the current TurnStart fixes the symptom without deleting previous records. Hosted and
   default-absence regressions pass; ordinary current-turn answers remain covered.
2. **Responses parsing preceded host counting.** `AssistantReplay::validate` parses raw
   function arguments, so deferring only `validate_response` was insufficient. A malformed
   Responses argument fixture failed before the model callback. The admitted path now defers
   both runner-side validations until after the host sees the moved complete event. Host
   refusal wins first; a host that accepts still reaches full domain rejection before copies
   or tools. Provider decoder parsing remains under its own original response bounds; this
   change does not claim admission before provider parsing or ordinary neutral assembly.
3. **Duplicate stream method during reconciliation.** The three-way merge initially retained
   the old method beside the extracted one. Compilation rejected the duplicate. The old
   method/helper were removed explicitly; current source has one stream path and all scopes
   compile. The source-history fitting improvements remain intact.
4. **Fixture lifetime accounting.** Installed record policy and reserved turn now have
   separate types, so policy teardown cannot masquerade as reservation release. Trace
   assertions cover original model acceptance, batch commit/drop and step/turn release.
5. **Consumer dependency scope.** The temporary manifest initially supplied an unused store
   patch. It was removed; the seven active Nanus dependencies are patched together for this
   fictional consumer only. Its locked test/lint evidence does not change production pins.

## Regression checks and evidence

Fourteen scoped gates pass with 249 runtime/config/test inputs unchanged: 539 affected
provider/domain/ports/store tests; 183 minimal-runner nextest cases; 53 all-feature admission/
embedding cases; two minimal-runner doctests; embedded default/providers 29/35; 342 runtime-
free TUI cases; workspace/minimal/TUI/downstream warning-denying Clippy; formatting; and
Windows MSVC-target domain/ports/minimal-library Clippy. Suites overlap; these counts are not
summed. Ninety-four functions in the eight reviewed record/transport fixture files are at most
60 lines. The six actual HTTP fixtures include stock and hosted paths, opaque-only continuation,
truncated completion, redirect refusal and host refusal of a valid completed response.

Five actual Hype record-capacity consumer cases pass against this combined candidate,
including near-limit retained history and dropped pending model work. Locked consumer Clippy passes. Hype's full `pnpm verify` also exits 0: 1083 Rust and
132 frontend tests pass, zero fail; 17 explicit Rust native/manual acceptance tests remain ignored.
Logs, exact gate commands, frozen hashes, merge inputs, isolated manifest/lockfile and function
review are under `/private/tmp/hype-record-responses`.

Stock credential-aware composition tests and live provider calls are deliberately unrun;
no real Keychain, personal Chrome or paid API is used. Windows compilation is not native
execution. Original physical cleanup, all-tool/goal authority, live providers, production
adoption, read_video/paid analysis and platform/package/recovery acceptance remain open.

## Verdict

**LIKELY_CORRECT for the reconciled local mechanism; high confidence.** Both reproduced
behavioral findings are fixed and covered through actual runner paths. This certificate does
not establish immutable publication, original Hype production admission, live/native acceptance
or completion of the app migration.
