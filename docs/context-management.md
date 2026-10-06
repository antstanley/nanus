# Managed context

Managed context is an **opt-in, per-session** way of building the request a model is sent. The
legacy fitter drops whole old user turns once a conversation outgrows its budget; managed context
instead keeps **every user message**, selects among the model's own completed work, keeps
bounded source-backed working notes, lets the model read earlier evidence back, and checkpoints
every accepted decision before a request uses it. The raw session log stays the only authority:
the effective request is a derived view, and nothing rewrites, reorders or deletes an event.

A session that never enables it is read, encoded, run and saved exactly as before — same request
bytes, same tool count, same event trace, same terminal save. Two changes apply to every session
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

`ContextPolicy` has four fields: `mode` (`legacy` | `managed`), `output_reserve_tokens`,
`capture_shell` and `policy_version`. The defaults are legacy, 4,096, false and 1. Capture is
valid only in managed mode, and nothing turns managed mode on implicitly.

A policy change is a human or host action taken while the session is idle. It is recorded as a
`context/mode` record and checkpointed before it is acknowledged. Enabling makes the body managed
through the checkpoint — version 3, or version 4 marked managed when it holds typed user content;
disabling never downgrades it and never deletes past records. Activation refuses — without changing the session — when the selected model path has no
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

A **frontier** is a session id, an exclusive event count, the accepted revision, and the BLAKE3
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
request's notice reports, and once to freeze it. The runner's own call assembler and the adapters'
decoders apply the context tools' raw argument limits once a call's name is known — including a
name that arrives after its arguments — and before anything is parsed.

The supported matrix is recorded with the provider work (see [Providers](#provider-matrix) below).
Stateless and ordinary Responses paths stay unsupported: activation and provider switches refuse
before HTTP, with no wire, model or effort switch and no opaque item deleted or rewritten.

A model or provider switch to an unsupported path makes the session unready; the next request is
refused before HTTP, the shared selection is not reverted, and the host chooses a supported
selection or resets the session's context.

### Provider matrix

Each supported path splits its dispatch into one encode and a separate send, so the ordinary
path's bytes are unchanged and the prepared call sends exactly what it estimated and digested.
Support requires the official endpoint and a known model; anything else is unsupported.

| Provider | Endpoint and plan | Protocol label | Models | Support |
|---|---|---|---|---|
| DeepSeek | `https://api.deepseek.com` | `deepseek.chat` | `deepseek-flash`, `deepseek-v4-pro` | Supported |
| OpenAI | API plan, `https://api.openai.com/v1`, with an exact Chat Completions preference | `openai.chat` | The offered models whose chat tool support is not refused (`gpt-6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`) | Supported |
| OpenAI | Any id the vendor does not offer, which has no declared output ceiling | — | — | Unsupported |
| OpenAI | Anything routed to Responses: automatic `gpt-5.6`+, the `subscription` plan, exact Responses, stateless Responses | — | All | Unsupported; refused before HTTP |
| z.ai | API plan, `https://api.z.ai/api/paas/v4` | `openai.chat` | The known API models | Supported |
| z.ai | Coding Plan, gateways | — | — | Unsupported |
| Anthropic | `https://api.anthropic.com/v1` | `anthropic.messages` | `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-fable-5-1` | Supported |

The output reservation must be present and within the model's declared ceiling; it is refused,
never clamped. Every adapter validates the managed role grammar — leading system messages only,
the generated message, when present, directly after the first user message and never last, and
every surviving call paired with exactly one result. Generated data is identified by that
position; a model reply elsewhere that opens with the same label is sent as the reply it is.
Under managed preparation the chat decoders also bound a call's arguments before its name is
known, at one record.

Anthropic signed replay is sent only when the adapter's existing check accepts it: the original
blocks under the encoded prefix that produced them. When hiding changes a turn's prefix, that turn
is sent in its neutral form — its text and tool uses, without the thinking blocks — which the
Messages API accepts as an edited history; signatures and digests are never rewritten. A turn that
carries only signed replay and has no neutral form refuses preparation. Because the notice changes
every request, managed Anthropic sessions usually send their history neutrally: that costs the
earlier reasoning and the prompt cache, never correctness.

These rest on wire and reload fixtures — a local server receives exactly the prepared bytes — not
on live provider evidence; see [what is verified](#what-is-and-is-not-verified).

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
record, a finished `unknown_dispatch` attempt for every intent with no outcome, and an
interrupted `TurnEnd`, checkpointed. Nothing is rerun, and an unknown dispatch is never counted as
paid usage. A crash during a tool batch can leave effects newer than the last checkpoint; this
design does not promise exactly-once external effects.

Version 3 adds six records — `context/mode`, `context/revision`, `context/decision`,
`artifact/published`, `request/attempt`, `context/recovery` — each validated on write and on read.
A managed session is version 3, or version 4 with `"managed":true` in its header when it also
holds typed user content. Readers accept versions 1 to 4; a body that is not managed and carries
one of these records is refused, and a future version is refused before anything is mutated.

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

## Human controls and clients

The command line takes the three flags in [configuration](#configuration-activation-and-limits).
A managed session run by `nanus run` is saved only through its checkpoints; the command reports
an unsaved or uncertain outcome on stderr, keeps stdout for the answer, and exits non-zero.

The link adds a `context` request with `status` and `reset` actions on a held session, and the
interface `/context` and `/context reset`. A reset while a turn runs is refused; a status read
during a turn is answered from the snapshot the turn last published, never by borrowing the
running session. A reset selects legacy, appends an empty host revision and persists both before
it is acknowledged. Status carries the mode, readiness and its reason, the revision and frontier,
the profile digest, the last decision, the hidden and protected counts, the estimate and its
estimator, the reservation and whether recall and the archive are available — never credentials,
provider payloads or evidence.

`ContextStatus`, `ContextDecision` and `Checkpoint` frames carry the session id, the stream epoch,
the turn and step (null when idle), a frame id assigned by the session, the stream watermark and
the durable frontier. Attachment and backlog semantics are described in
[sessions](sessions.md#what-travels-over-the-link); the link protocol is version 10. The interface
draws context notices with the replay's own wording, so a watched turn and the same turn read
back say the same things. A managed session whose store cannot checkpoint, or whose recovery or
reconciliation could not be settled, is held but takes no turn until it is reset.

## Accounting

Every managed request records a `request/attempt` intent before HTTP and a finished record when it
settles, durable by the next checkpoint: the exact selection identity, the projection revision, the digest of the body
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

Verified by deterministic tests against the real runner, store, shell, link, interface and the
three adapters' encoders and decoders, on macOS:

- legacy sessions keep their request bytes, schemas, event trace and save path (each adapter pins
  its ordinary body; the runner compares a legacy run through both entry points);
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
  the real link and store.

Not verified, and not claimed:

- **No live provider has been sent a managed request.** The provider matrix rests on wire and
  reload fixtures; in particular, nothing yet proves a provider accepts the generated assistant
  message followed by an original one, or DeepSeek and z.ai a generated turn without
  `reasoning_content`.
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
