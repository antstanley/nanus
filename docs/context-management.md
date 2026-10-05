# Managed context

Managed context is an **opt-in, per-session** way of building the request a model is sent. The
legacy fitter drops whole old user turns once a conversation outgrows its budget; managed context
instead keeps **every user message**, selects among the model's own completed work, keeps
bounded source-backed working notes, lets the model read earlier evidence back, and checkpoints
every accepted decision before a request uses it. The raw session log stays the only authority:
the effective request is a derived view, and nothing rewrites, reorders or deletes an event.

A session that never enables it is read, encoded, run and saved exactly as before — same request
bytes, same schemas and tool count, same event trace, same terminal save. The one deliberate
exception, for every session, is [deletion](#deletion-and-retirement).

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

`ContextPolicy` has four fields: `mode` (`legacy` | `managed`), `output_reserve_tokens`,
`capture_shell` and `policy_version`. The defaults are legacy, 4,096, false and 1. Capture is
valid only in managed mode, and nothing turns managed mode on implicitly.

A policy change is a human or host action taken while the session is idle. It is recorded as a
`context/mode` record and checkpointed before it is acknowledged. Enabling upgrades the session
body to version 3 through the checkpoint; disabling never downgrades it and never deletes past
records. Activation refuses — without changing the session — when the selected model path has no
managed preparation, when its declared output ceiling is below the reservation, when a
registered tool already uses a context tool's name, or, for capture, when there is no archive or
the shell cannot capture.

`nanus run` takes `--context-mode legacy|managed`, `--context-output-reserve <tokens>` and
`--capture-shell-evidence`. Absent flags keep a resumed session's recorded policy, or legacy for
a new one; a capture or reserve flag on a new session without `--context-mode managed` is refused.

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

## Fragments, frontiers and revisions

A **fragment** (`f:<seq>`) is one retained assistant message — the sequence of its event — and
every settled tool result that answers its calls, paired by call id rather than by position.
Text-only assistant messages are fragments too. A fragment is settled once every call is answered
or its turn has ended. Calls and results are atomic: sibling calls are never split and a call is
never shown without its result. Derivation refuses a log whose call identity is ambiguous — an id
carried twice, answered twice, or answered with no call — rather than inventing a grouping.

A **frontier** is a session id, an exclusive event count, the accepted revision, and the SHA-256
of the stored header and the first `event_count` event lines, byte for byte as the session
serializer writes them (`Session::prefix_digest`). At the full count it is the digest of the
whole stored file, which is what lets a store recompute it from disk without re-encoding.

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
3. Add a fixed-schema notice as a second system message: counts, revision, estimate and its
   estimator, allowance, pressure, goal phase and revision, recovery availability. Only numbers,
   enums and opaque ids — nothing retrieved or model-written is promoted into a system message.
4. Render accepted notes, the goal objective and the recovery catalog as one labelled
   **assistant** text message, with no calls, reasoning or replay, immediately after the
   earliest user message.
5. Never make that message the last one: a fresh request with nothing after its first user
   message carries no generated data, and the notice says `goal_data_available=false` and points
   at `get_goal`.

Deterministic hard fitting acts only when the candidate is over the hard allowance, hiding the
oldest eligible complete fragment first. Every probe is compiled and prepared by the selected
adapter, so its cost is the cost of the body that adapter would send. A protected floor above
60 % but within the allowance succeeds; above the allowance it refuses with
`protected_floor_too_large`, and no partial hidden set is installed. A successful fit is an
automatic revision persisted with the request intent before dispatch. After a reset to legacy,
the legacy whole-turn fitter applies again, with its existing notice.

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
checkpoint the candidate revision or the unchanged one + RequestAttempt(finished)
install what was acknowledged -> honour cancellation -> release the selection
next step, or append and checkpoint TurnEnd before the answer is reported
```

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
performs no I/O until `stream` is polled. The runner's own call assembler and the adapters'
decoders apply the context tools' raw argument limits once a call's name is known — including a
name that arrives after its arguments — and before anything is parsed.

The supported matrix is recorded with the provider work (see [Providers](#provider-matrix) below).
Stateless and ordinary Responses paths stay unsupported: activation and provider switches refuse
before HTTP, with no wire, model or effort switch and no opaque item deleted or rewritten.

A model or provider switch to an unsupported path makes the session unready; the next request is
refused before HTTP, the shared selection is not reverted, and the host chooses a supported
selection or resets the session's context.

### Provider matrix

Filled in from the adapters' own `managed_support` once their preparation and wire fixtures land.

## Checkpoints and recovery

A managed turn saves through a `SessionCheckpoint` the host binds to its writer claim, store and
session (`StoreCheckpoint` in the stock composition). A commit names the stored identity it
replaces — the actual file's body version, digest and event count, never a re-encoding — and
returns a receipt whose frontier covers the whole written file.

| Outcome | Meaning | What the turn does |
|---|---|---|
| Acknowledged | The candidate is on disk | Installs it; watchers move their watermark |
| `NotCommitted` | The previous file is intact | Leaves the projection uninstalled, stops new effects, makes one reserved terminal attempt |
| `CommitOutcomeUnknown` | Replacement may have happened | Reads the disk back under the same claim: the candidate installs, the previous file keeps the old projection, anything else freezes the session |

`run_turn_with_runtime` returns the ordinary result **and** a `PersistenceState`, on failure too.
Hosts never save a version-3 session themselves: `Acknowledged` permits no duplicate save,
`Unsaved` permits nothing beyond the runner's one terminal attempt, and `Unknown` permits only
reconciliation. Checkpoint success guarantees process-crash atomic replacement; the stronger
power-loss grade is not claimed.

A resumed session with an open turn is closed before new work is admitted: an `context/recovery`
record, a finished `unknown_dispatch` attempt for every intent with no outcome, and an
interrupted `TurnEnd`, checkpointed. Nothing is rerun, and an unknown dispatch is never counted as
paid usage. A crash during a tool batch can leave effects newer than the last checkpoint; this
design does not promise exactly-once external effects.

Version 3 adds six records — `context/mode`, `context/revision`, `context/decision`,
`artifact/published`, `request/attempt`, `context/recovery` — each validated on write and on read.
Readers accept versions 1, 2 and 3; a version-1 or version-2 body that carries one of them is
refused, and a future version is refused before anything is mutated.

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

Filled in with the capture and archive work.

### Deletion and retirement

Filled in with the store work.

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

## Human controls and clients

Filled in with the link and interface work.

## Accounting

Every managed request records a `request/attempt` intent before HTTP and a finished record at its
settled checkpoint: the exact selection identity, the projection revision, the digest of the body
actually dispatched, the outcome (`completed`, `failed`, `cancelled`, `refused`,
`unknown_dispatch`), the management fragment it carried, and nullable usage. Missing is never
zero; a usage report followed by a stream error survives as the failed attempt's usage; repeated
cumulative reports replace the attempt's snapshot rather than accumulate. Monotonic timings
(response head, first token, decode, total) are measured with the process clock and never
derived from wall-clock timestamps.

## Errors

Refusals carry one of the stable codes `unsupported_mode`, `policy_denied`, `stale_base`,
`invalid_fragment`, `protected_fragment`, `invalid_reference`, `candidate_too_large`,
`protected_floor_too_large`, `protocol_incompatible`, `storage_capacity`,
`checkpoint_not_committed`, `checkpoint_unknown`, `cursor_expired`, `source_unavailable`,
`source_corrupt`, `capture_partial`, `capture_unavailable`, `cancelled` and
`mixed_mutation_batch`, with bounded ids and counts and never source excerpts. A malformed tool
argument is an ordinary bounded tool failure; a failed context edit never invalidates an earlier
result or retries a command.

## What is and is not verified

Filled in when the gates have run.
