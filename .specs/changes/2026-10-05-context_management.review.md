# Semi-formal review: bounded, recoverable context management

**Date:** 2026-10-06 · **Scope:** branch `context-management` at `66fd639` compared with `main`
(`e252e6b`), about 113 files and +30k lines. The change spec is
`clm-research/.specs/changes/2026-10-05-context_management.md` (CM-01 … CM-13, T01–T35). The
implementation's own contract is [`docs/context-management.md`](../../docs/context-management.md).
This is a read-only review. Findings were confirmed by reading the code and, where it was cheap,
by throwaway tests in detached worktrees outside the checkout. Those worktrees were removed, and
the repository is unchanged apart from this file.

Each finding is marked **CONFIRMED** (traced end to end, or reproduced) or **PLAUSIBLE** (strong
trace, not reproduced), with a severity. Findings are listed most severe first.

## Premises

P1. CM-04: hard fitting hides the oldest eligible fragments until the candidate fits, and
refuses with `protected_floor_too_large` only when the protected floor alone is over the
allowance (T02, T04).

P2. CM-08: an acknowledged checkpoint means the candidate is on disk. `NotCommitted` means the
previous file is intact. After an unknown outcome, only the *exact candidate* digest may be
installed, and there is no alternate overwrite.

P3. CM-09: recall cursors bind an immutable snapshot, and "appends do not move an existing search
snapshot". Partial coverage is honest.

P4. CM-05: the inspect catalog is paginated under 8 KiB, with a cursor bound to the revision,
profile and frontier.

P5. CM-01 / CM-08: existing sessions stay legacy until explicitly enabled, and v3 is written only
after successful activation. CM-07: support is granted only for exact
protocol/model/endpoint combinations with a usable declared output ceiling.

P6. CM-06 transaction order: reserve capacity → intent checkpoint → StepStart/HTTP → … → batch
lease commit → StepEnd → evaluate staged proposal → settle checkpoint. CM-12: every intent has an
outcome, and missing counters are null, not zero.

P7. CM-13: a legacy session with no new runtime hooks keeps its request encoding, tool schemas
and event trace. The doc's claim (`docs/context-management.md:10-11`) is "same request bytes,
same schemas … same event trace".

## Findings

### F1 — high — CONFIRMED: hard fitting never runs against a real adapter

- **Where:** `crates/nanus-bundle/src/agent_loop/managed/prepare.rs:261-270` (cost closure),
  `:227-238` (`prepare_call` turns every adapter error into `ProtocolIncompatible`),
  `crates/nanus-domain/src/context/managed/fit.rs:57`.
- **Defect:** the fit measures each candidate by preparing it with the adapter. Every shipped
  adapter refuses an over-budget candidate before returning an estimate, so the fit's very first
  probe fails.
- **Certificate:**
  - The DeepSeek (`managed.rs:153`), OpenAI (`:181`) and Anthropic (`:193`) `prepare` all return
    `Err(CandidateTooLarge)` when `!estimate.fits(caps, &request)`.
  - `RequestEstimate::fits` (`nanus-ports/src/capabilities.rs:158-170`) checks input +
    reservation ≤ min(`context_budget`, window). That is the runner's own allowance, because
    `base_request` sets `context_budget` (`prepare.rs:171`).
  - `hard_fit` calls `cost(accepted)?` first (`fit.rs:57`) and only fits when that cost is over
    the allowance, which is exactly when the adapter refuses.
- **Trace:** the session outgrows its budget → `cost` fails with `Err(ProtocolIncompatible)` →
  `prepare_step` refuses → `TurnEnd(Error)`. This repeats on every later turn.
- **Failure scenario:** a long managed session gets stuck with "the selected provider and model
  cannot carry a managed context". Nothing is ever hidden (T02), and an oversized floor reports
  `protocol_incompatible`, not `protected_floor_too_large` (T04). The proposal dry run in
  `settle.rs:229` goes through the same path.
- **Reproduced:** I made the test double (`tests/managed_context/harness.rs`) refuse
  `!estimate.fits(..)` the way the adapters do. Then
  `automatic_fitting_hides_old_fragments_and_keeps_every_user_message` fails, and
  `a_protected_floor_over_the_budget_refuses_before_any_request` reports `ProtocolIncompatible`
  instead of `ProtectedFloorTooLarge`. The shipped double never refuses, which is why the suite
  passes.
- **Fix direction:**
  - Give probes an estimate-only preparation that does not apply `fits`, or have the adapter
    return its estimate inside the refusal.
  - Keep the adapter's error code instead of flattening it.
  - Make the test double refuse the way the real adapters do.

### F2 — high — CONFIRMED: a checkpoint can be acknowledged with a truncated file on disk

- **Where:** `crates/nanus-adapter-store/src/store/checkpoint.rs:287-291` (`write_synced`),
  called by `replace` (:193-208). The same pattern exists in the legacy `write_atomic`
  (`store.rs:1159-1182`), so it predates this change but now backs every checkpoint.
- **Defect:** `tokio::fs::File::write_all` returns as soon as the blocking write has been
  spawned. `sync_all` waits for that write but only *stores* its error, and returns just the
  fsync result (tokio 1.53 `fs/file.rs:317-323`). Only `flush()` returns the stored error
  (`:1104-1107`). So a failed final write returns `Ok`, the file is renamed into place, and a
  receipt is returned.
- **Failure scenario:**
  1. ENOSPC, EFBIG or EIO hits the last write chunk (bodies under 2 MiB are a single chunk).
  2. The intact previous file is replaced by a truncated, unloadable one.
  3. The runner records `Acknowledged`, and `StoreCheckpoint` advances `expected` to a digest
    that is not on disk.
- **Reproduced:** with RLIMIT_FSIZE at 8000, a 20 KB candidate returned `Ok(event_count=5)`. The
  file on disk was 8000 bytes, and `load` then failed with "truncated tail".
- **Fix direction:**
  - Call `file.flush().await?` before `sync_all()`, or write with std I/O inside
    `spawn_blocking`.
  - Apply the same fix to `write_atomic` and to the sink in F6.

### F3 — high — CONFIRMED: recall search cursors are re-sealed at the live count, so long archives never finish

- **Where:** `crates/nanus-bundle/src/recall.rs:140-155` (`seal` writes
  `scope.frontier.event_count`), `:207` (sources rebuilt at the sealed count) and `:440-458`
  (`resume_at` indexes the live list). `run_recall` takes the frontier from the live session
  on every call (`managed/tools.rs:293-300`).
- **Defect:** the follow-up cursor carries the *current* count but an index into the source list
  built at the *original* count. Event sources come before artifact sources, so every append
  shifts the artifact indexes.
- **Trace:**
  1. Page 1 runs at count N.
  2. k events are appended.
  3. Page 2 is correct but is re-sealed at N+k.
  4. Page 3 rebuilds the list at N+k. Its index now names a new event source, so the artifact is
    rescanned from byte 0.
- **Reproduced:** a 704 KiB artifact with a marker at byte 10 000, plus one user message
  appended between pages. Pages 2 to 6 each returned the same hit, always with a cursor. The
  search never completes and never reaches anything past about 512 KiB.
- **Fix direction:**
  - Re-seal with the snapshot count taken from the cursor.
  - Make `resume_at` use the same snapshot list.
  - Check the sealed prefix tag.

### F4 — medium — CONFIRMED (host side reproduced, runner side traced): the link's reconciliation installs a receipt for a session it does not hold

- **Where:** `crates/nanus-link/src/server/context.rs:298-347` (`settle`, `receipt_from`),
  used by `managed_ending` (:360-373).
- **Defect:** after an `Unknown` outcome, the host builds a receipt from its *in-memory* session
  whenever the disk holds the runner's candidate. It never checks that
  `session.prefix(event_count)` hashes to that file.
- **Certificate:**
  - `record_intent` assigns `*session = candidate` only after a successful commit
    (`step.rs:283-285`).
  - The runner's `reconcile` maps a failed disk read (`Err(_)`) to `Frozen` and
    `Unknown{candidate_sha256}` (`managed.rs:484-490`).
  - `drive_managed` then appends `TurnEnd`, and the frozen TurnEnd commit performs no I/O.
  - The host's `binding.checkpoint.reconcile` succeeds. `StoreCheckpoint` sets `expected` to the
    candidate's identity (`checkpoint.rs:103-106`).
  - `receipt_from` only does `session.prefix(event_count).ok()?`. The in-memory history (prior
    events + `TurnEnd`) and the disk (prior events + `RequestAttempt(started)`) have the same
    count, so `held.saved(..)` runs and `Done`/`Failed` is sent.
- **Failure scenario:** a transient read error during the runner's reconciliation of an intent
  commit that actually landed. The host marks the session saved. Its next commit passes the
  store's compare-and-swap and overwrites the durable intent record, which rewrites history.
- **Fix direction:**
  - Require `session.prefix_digest(event_count) == file_sha256` in `receipt_from`, and
    quarantine otherwise.
  - Better, carry the uncertain candidate itself through `PersistenceState::Unknown`.

### F5 — medium — CONFIRMED (reproduced): the `context_manage inspect` cursor always expires in the next step

- **Where:** `crates/nanus-bundle/src/agent_loop/managed/tools.rs:180-195` (binding
  `c1:{revision}:{frontier.event_count}:{profile}`) and `:149-168` (`live_status` moves the
  frontier to the current count).
- **Defect:** a cursor is used one step after it was issued, and every step appends events, so
  the bound count never matches.
- **Reproduced:** 63 fragments; the first page returned 40 with a cursor at count 451. The next
  step's inspect at 458 returned `cursor_expired`. Descriptors are oldest first
  (`proposal.rs:281`), so the newest fragments beyond page one can never be listed.
- **Fix direction:** bind the cursor to the frontier it was issued at (count plus prefix digest,
  which an append-only log keeps verifiable), and page over that snapshot. Add a cross-step
  pagination test.

### F6 — medium — CONFIRMED (reproduced): a capture write error gives a "complete" receipt and then blocks every save in the turn

- **Where:** `crates/nanus-adapter-store/src/store/sink.rs:205-221, 255-275` (`FileSink::flush`
  and `seal`: the same `write_all`/`sync_all` gap as F2).
- **Trace:**
  1. The last staged write fails silently, and the object is renamed and receipted `Complete`
    with the full `retained_bytes`.
  2. `publish_captures` queues the proof (`managed/tools.rs:383`).
  3. The next checkpoint verifies the object, finds the length wrong, and returns
    `NotCommitted(SourceCorrupt)`.
  4. The single reserved terminal attempt sends the same artifact list, because it is cleared
    only on acknowledgement (`managed.rs:418, 442`), so it fails too.
- **Failure scenario:** one disk error in the best-effort archive leaves the whole turn unsaved,
  including tool effects that already ran. CM-09 says a sink failure makes the receipt
  partial or unavailable instead.
- **Reproduced:** with RLIMIT_FSIZE at 70000, a 100 000-byte capture gave `Complete/100000`;
  `verify` returned `Corrupt` and the checkpoint `NotCommitted(SourceCorrupt)`.
- **Fix direction:**
  - Flush within the deadline before `seal`, and downgrade the receipt on error.
  - In the runner, turn an artifact that fails verification into an unavailable receipt rather
    than refusing the checkpoint.

### F7 — medium — CONFIRMED (reproduced): a legacy-mode policy change upgrades a session to v3 for good

- **Where:** `crates/nanus-bundle/src/agent_loop/managed/settle.rs:251-272` (the early return
  only when `current == policy`; `upgrade_to_managed_body()` runs unconditionally), and
  `crates/nanus-cli/src/cli.rs:1036-1076`.
- **Scenario:** `nanus run --resume <v2 session> --context-output-reserve 8000 …`.
  - The reserve flag is refused only for new sessions.
  - `wanted != current`, so the CLI calls `set_context_policy`.
  - Activation checks are skipped because the mode is legacy, but the body is upgraded and a
    `context/mode` record is checkpointed.
  - The session is now v3 in legacy mode. Older binaries cannot read it, and disabling never
    downgrades it.
  - This breaks P5.
- **Reproduced:** `set_context_policy` with `{mode: Legacy, output_reserve_tokens: 8000}` on a
  fresh v2 session returned `Acknowledged`, and `is_managed_body()` was then `true`.
- **Fix direction:**
  - Upgrade only on activation.
  - Refuse, or do not record, legacy-mode policy changes on a non-v3 body.
  - Refuse `--context-output-reserve` without managed mode on resume as well.

### F8 — medium — CONFIRMED (reproduced): OpenAI managed support is granted for unknown models with no declared ceiling

- **Where:** `crates/nanus-adapter-openai/src/managed.rs:58-79` (`support`: `tool_support`
  returns `Unknown`, not `Unsupported`, for ids outside its exact list) and `:166-170`
  (reservation checked against the 128 000 vendor fallback), together with
  `crates/nanus-bundle/src/agent_loop/managed/prepare.rs:133-139` (`is_some_and` passes when
  `max_output_tokens` is `None`).
- **Failure scenario:** `gpt-3.5-turbo`, `o1-mini`, `gpt-4o-mini` or a fine-tune id all report
  `Supported{1}` and prepare with a 100 000-token reserve. The request is sent after the intent
  checkpoint, and the provider rejects or clamps it.
- **Contradicts:** CM-07 and CM-01 ("if the endpoint cannot establish a usable ceiling, managed
  activation is unsupported"), and the doc's "a known model".
- **Fix direction:**
  - Use an exact model allowlist with declared ceilings, as the other adapters do.
  - Have `unready` refuse when `max_output_tokens` is `None`.

### F9 — medium — CONFIRMED (reproduced): recall can skip a match and then report complete coverage

- **Where:** `crates/nanus-bundle/src/recall.rs:299-308` (`overlap.max(start + 1)`).
- **Defect:** when the work budget leaves a window shorter than the needle (including an empty
  window at exactly 256 KiB examined), the resume point is forced to `start + 1`. A match that
  starts at `start` is lost.
- **Reproduced:** message 1 is 262 144 × `a`; message 2 starts with the needle. Page 1 has no
  hits and is partial with a cursor. Page 2 has no hits and reports `complete`.
- **Fix direction:** enforce `start + 1` only when the window examined at least the needle's
  length; otherwise resume at `start`.

### F10 — medium — CONFIRMED (by reading): missing usage counters are recorded as zero

- **Where:** `crates/nanus-domain/src/context/managed/records.rs:631-648`
  (`UsageObservation::from_usage`), called from `settle.rs:158`.
- **Defect:** the neutral `Usage` has non-optional counters, and `from_usage` wraps
  prompt/completion/reasoning/cache-read in `Some(..)` unconditionally.
- **Failure scenario:** a provider with no cache breakdown is recorded as `cache_read_tokens:
  Some(0)`. CM-12 and T31 require "missing is not zero".
- **Fix direction:** carry presence through the adapters' usage decoding, or record `None` for
  counters the provider did not report.

### F11 — low — CONFIRMED (reproduced): a staged proposal is accepted when its batch lease commit failed

- **Where:** `crates/nanus-bundle/src/agent_loop/managed/settle.rs:68-75`. It cancels only on
  stop or `Interrupted`, and evaluates for any other result, including `Err`.
- **Trace:** with a `ToolAdmission` installed, `lease.commit` refuses (`agent_loop.rs`,
  `run_tools`) → `perform_managed` returns `Err` → `settle` → `evaluate` → `ContextRevision`
  and `ContextDecision{Accepted}` are checkpointed.
- **Reproduced:** decision `p-35` was `Accepted` with the turn outcome "tool admission failed:
  commit refused".
- **Contradicts:** CM-06 ("after the existing batch lease commits and StepEnd is appended, the
  runner validates…") and T16.
- **Fix direction:** evaluate only when the step result is `Ok(ToolCalls)`. Otherwise record
  `rejected` or `cancelled`.

### F12 — low — CONFIRMED (reproduced): the intent is checkpointed before step capacity is reserved, leaving a dangling durable intent

- **Where:** `crates/nanus-bundle/src/agent_loop/managed/step.rs:164-173`. `record_intent`
  commits before `begin_managed_records` calls `reserve_step`. The doc's own order is "reserve
  record capacity → checkpoint … RequestAttempt(started)".
- **Failure scenario:** with record admission installed, `reserve_step` refuses after the intent
  is durable. The step returns `Err` before `settle`, and the closed turn is checkpointed with a
  `request/attempt started` that has no outcome. Recovery only handles open turns, so the intent
  is never answered. A failing `end_step_records` (`step.rs:186`) skips `settle` in the same way.
- **Reproduced:** stored attempts were started=1, finished=0, with `TurnEnd(Error)` and nothing
  sent.
- **Fix direction:** reserve before the intent checkpoint, and record a finished `refused`
  attempt on every error path after the intent.

### F13 — low — CONFIRMED (by reading): recovery and idle operations misreport or drop the persistence state

- **Where:**
  - `crates/nanus-bundle/src/agent_loop/managed.rs:236-243`: any recovery failure is reported as
    `Unsaved{last}`, even when the recovery commit's outcome was unknown and reconciliation froze.
  - `settle.rs:281-284, 362-366, 393-396`: `set_context_policy`, `reset_context` and
    `recover_session` return only `Err`, so their `PersistenceState` is lost on failure.
- **Effect:** the CLI prints "the copy saved before this turn is intact" for an unknown outcome,
  and hosts cannot tell `Unknown` from `Unsaved`. This breaks CM-08 ("returns … PersistenceState,
  including on failure"). The store's compare-and-swap still prevents a blind overwrite.
- **Fix direction:** propagate the inner `ManagedTurn::persistence()` on every error.

### F14 — low — CONFIRMED (by reading): legacy `read`/`grep` schemas and results changed

- **Where:**
  - `crates/nanus-bundle/src/tools/read.rs:79-121`: new `byte_offset`/`max_bytes`/`version`
    properties and description.
  - The changed refusal text for files over 4 MiB.
  - `crates/nanus-bundle/src/tools/grep.rs`: new `skipped_*`/`coverage` keys.
- **Defect:** these changes are unconditional, so a legacy session sends a different schema than
  `main` and records different results. That breaks P7 and the doc's "same request bytes, same
  schemas". CM-09 asks for `read` to be extended, so the spec is internally in tension, but the
  doc's T01 claim is inaccurate either way, and no test compares the stock schema with `main`.
- **Fix direction:** either gate the new fields to managed sessions, or declare this a deliberate
  exception next to deletion and correct the doc and the T01 wording.

### F15 — low — CONFIRMED (reproduced): the generated message is recognised by its text label, so a model can wedge its session

- **Where:** `is_generated` / `validate_sequence` in `nanus-adapter-deepseek/src/managed.rs:209-263`,
  `nanus-adapter-openai/src/managed.rs:231-285` and `nanus-adapter-anthropic/src/managed.rs:243-295`.
- **Failure scenario:** an original assistant reply that begins with `MEMORY_LABEL`, through
  mimicry or injection, counts as a second generated message. Every later request is refused
  with `protocol_incompatible`. The fragment is recent and protected, so only a human reset gets
  out.
- **Fix direction:** identify the generated message structurally (by position or a marker the
  compiler passes), never by its content.

### F16 — low — CONFIRMED (by reading): fitting refuses at the 4,096-id cap even when a smaller fit exists

- **Where:** `crates/nanus-domain/src/context/managed/fit.rs:81-85`. It computes
  `count = settled.max(crossing)` and then checks the cap.
- **Defect:** if crossing the allowance needs no more than 4,096 hides but the 60 % target needs
  more, the whole fit fails with `storage_capacity`, against "stop at the target or at the
  protected floor".
- **Fix direction:** clamp the target count to the remaining cap. Refuse only when `crossing`
  itself exceeds the cap.

### F17 — low — CONFIRMED (by reading): note text can forge harness-looking lines

- **Where:** `crates/nanus-domain/src/context/managed/notes.rs:20-44` (no newline or control
  character check) and `compile.rs:261-269` (each note is rendered as `"\n- {id} [{cat}] {claim} …"`).
- **Failure scenario:** a claim containing `"\n\nGoal (revision 9, active, set by host): …"` is
  rendered as a host-looking goal line inside the generated message. T07 asks that model
  inference stay labelled as model data.
- **Fix direction:** refuse control characters and newlines, or render each claim as an escaped
  JSON string.

### F18 — low — CONFIRMED (by reading): duplicate terminal attempts and repeated accepted decisions are not refused

- **Where:** `crates/nanus-domain/src/context/managed/state.rs:79-126` (`fold`) checks only that
  revision decision ids are unique.
- **Missing checks:**
  - a second finished record for one `attempt_id`;
  - a finished record with no started one;
  - a second `context/decision accepted` for one id.

  These are all semantic rules in the spec's schema section. `consumed()` and accounting would
  count the duplicates.
- **Fix direction:** track attempt and accepted-decision ids in `fold_strict`.

### F19 — low — CONFIRMED (by search): archive garbage collection is never called outside tests

- **Where:** `crates/nanus-adapter-store/src/store/gc.rs:49`.
- **Effect:** dead lease markers from crashed processes (charged at 2 × 8 MiB), orphans and
  `.partial` files stay charged for good. Repeated crashes exhaust the 128 MiB per-session and
  1 GiB per-store quotas, and every later capture becomes unavailable.
- **Fix direction:** run collection at a safe point (store open, session release), or document it
  as a host obligation.

### F20 — low — CONFIRMED (by reading): the link reports a quarantined session as ready, and its recovery hint cannot work

- **Where:**
  - `crates/nanus-link/src/server/context.rs:535-557`: status ignores `held.quarantine`, so it
    can show `managed_ready: true` while `start_turn` refuses every turn.
  - The `QUARANTINED` text (:98-100) suggests `/context reset`. But `reset_reserved` (:592-627)
    reuses the binding whose expected identity is stale, so the store refuses with `StaleBase`
    every time.
- **Fix direction:**
  - Force `managed_ready=false` with the reason while quarantined.
  - Rebind from disk before a reset, or change the hint.

### F21 — low — CONFIRMED (by reading): capture staging can reach about twice the 128 KiB bound

- **Where:** `crates/nanus-adapter-local/src/shell.rs:1099` (128 KiB semaphore), together with
  `store/archive.rs:196-201` and `store/sink.rs:106-133`. Each sink keeps its own 64 KiB buffer,
  and the permit is released once bytes reach that buffer.
- **Fix direction:** count sink buffers inside the same budget.

### F22 — low — CONFIRMED (by reading): fit probes and the frozen request carry different notices

- **Where:** `prepare.rs:256-260` (probe `NoticeFacts` with no estimate, estimator or hint) and
  `:326-344` (the final notice includes them; `CandidateTooLarge` when over the allowance).
- **Effect:** a candidate within a few tokens of the allowance passes the fit and then is refused
  at freeze, instead of hiding one more fragment. CM-04 says to include overhead "at every
  iteration".
- **Fix direction:** probe with the final notice's worst-case widths.

### PLAUSIBLE (low)

- **P-a.** `CaptureBroker` is keyed only by the model-chosen `ToolCallId`, and one broker serves
  every link session (`compose.rs:620`, `capture.rs:64-89`). Equal call ids in two concurrently
  running sessions can archive one session's output in another's directory and publish the wrong
  receipt. Key the broker by (session, call), or use one broker per turn.
- **P-b.** The CLI applies policy flags (`bind_context`, `cli.rs:951-961`) before
  `run_turn_with_runtime` recovers an open turn. A `context/mode` record is then checkpointed
  inside a crash-left open turn, against "admitted while idle" and "closed before new work".
  Recover first.
- **P-c.** Stock composition installs no adapter `ResponseLimits`, so arguments that arrive before
  a late tool name are buffered without bound in the DeepSeek and OpenAI accumulators. CM-05
  requires a bounded envelope before the name is known. Require limits for managed preparation.
- **P-d.** A checkpoint does not verify that the candidate's first N event lines equal the stored
  file's (`store/checkpoint.rs:152-174`). A misbehaving host could rewrite earlier events and
  slip an unproven `artifact/published` below the stored count.
- **P-e.** After a quiesce timeout, `sink.rs:230-237, 278-286` drops the lease guard while the
  blocking write still runs. The quota scan briefly under-counts a growing `.partial`.

## Coverage: checked and found correct

- **Transaction order (runner).**
  - Selection is held for every managed step through settle (`step.rs:152`).
  - The intent checkpoint (settled prefix + automatic revision + `RequestAttempt(started)`)
    precedes `StepStart` and HTTP.
  - The frozen `PreparedModelCall` is consumed by `stream()` without re-encoding.
  - `StepEnd` precedes the settle checkpoint, and `TurnEnd` is checkpointed before `Ok`/`Done`.
  - Commits are never raced against cancellation.
- **Refused and unknown commits.**
  - `NotCommitted` halts with exactly one reserved terminal attempt.
  - `Unknown` reconciles under the same claim: the candidate installs, the previous file is
    kept, anything else freezes with no further write.
  - Effects stay in memory, and the projection is not installed.
- **Mixed batches.** They are refused before approval, goal effects, admission or capture. A
  malformed or oversized `context_manage` counts as mutating.
- **Policy.** Context tools reach `ToolPolicy` with read/write access, with no goal-tool bypass,
  and go through complete-batch admission and `before_dispatch`.
- **Legacy loop (T01).**
  - `managed: None` paths are unchanged.
  - The empty `ToolArgumentLimits` are inert.
  - `hold_selection(false)` behaves as before.
  - v1/v2 sessions run `run_controlled` with persistence `None`, and hosts still record before
    `Done`.
  - F14 is the exception.
- **Domain.**
  - Fragment pairing is by call id, with ambiguity refused.
  - The protected set covers every user message, the recent two fragments, and the management
    fragment until a completed, checkpointed attempt consumes it.
  - Compile order and memory placement are right: never last, omitted on a fresh prompt.
  - Proposal compare-and-set covers base, frontier, prefix, profile and goal, and eligibility
    requires results below the base count.
  - The reset barrier holds.
  - `prefix_digest` equals the stored bytes.
  - v3 records are refused in v1/v2 bodies on both read and write.
  - Arithmetic is checked.
- **Store.**
  - Expected identity is compared with the actual file bytes.
  - Classification is right: refusals before the rename are `NotCommitted`; after the rename the
    disk is read back.
  - The 4 MiB / 64 MiB bounds hold.
  - The session claim is taken before the bounded quota lock.
  - Range reads verify every intersecting chunk.
  - Artifact ids are scoped UUIDs with `O_NOFOLLOW`.
  - Deletion takes exclusive ownership, writes a durable retirement marker and moves to the trash
    atomically. Stale save, claim, checkpoint and name are refused, and crash cleanup only moves
    forward.
- **Recall.**
  - Selectors address only exact UTF-8 strings.
  - The digest covers the whole source.
  - Encoded output is bounded, with base64 expansion counted.
  - Partial coverage with a cursor is returned even at zero hits.
  - No workspace substitution happens.
  - `durable` labelling is right.
- **Capture.**
  - Sinks start before spawn, and exact bytes go to them before the preview cap.
  - Streams are separate.
  - Pumps never deadlock.
  - `Complete` is reported only at EOF with every byte retained.
  - Process-group and Job paths are shared with `run`.
  - `bash` output with no lease is byte-identical to `main`.
- **Link and TUI.**
  - Managed bodies are never saved through `StorePort::save`.
  - Backlog retirement on checkpoint is one synchronous mutation.
  - The attach barrier and watermark are right.
  - Epoch and frame-id deduplication work.
  - Clients clip to the advertised prefix and refuse a mismatch visibly.
  - The protocol is version 10, with mismatch refusal.
  - Reset is refused while busy, and status reads the published snapshot.
- **Providers.**
  - Each provider encodes once, with the estimate, digest and sent body identical.
  - No credentials appear in `Debug`.
  - Responses, the subscription plan, exact Responses, the z.ai Coding Plan and proxies are all
    refused before HTTP.
  - The reservation is refused, never clamped (F8 is the exception).
  - Role grammar is validated.
  - Anthropic sends signed replay only for an unchanged prefix, otherwise a neutral form, and
    never forges anything.
  - Pre-parse argument limits work, including a late name and Anthropic `input_json_delta`.
- **CLI.**
  - A v3 session never reaches `record` or `save`.
  - Unsaved or unknown outcomes are reported on stderr with a non-zero exit, and stdout stays the
    answer.
  - The session is named only after `Acknowledged`.

## Verdict

**CONCERNS — high confidence.**

- The transaction skeleton, refusal paths, legacy isolation and store identity are sound.
- Three high findings block the feature as specified:
  - F1: automatic fitting is inert against every real adapter, and the suite's test double hides
    this.
  - F2: a failed final write can be acknowledged as a durable checkpoint.
  - F3: recall pagination over archives loops.
- F4–F10 break MUST-level rules of CM-01, CM-05, CM-07, CM-08, CM-09 and CM-12, and each has a
  narrow fix.
- The documentation's "what is verified" list (`docs/context-management.md:372-391`) should not
  claim hard fitting, recall cursors or checkpoint atomicity as verified until F1–F3 are fixed
  and covered by tests whose doubles refuse the way the adapters do.

## Resolution — 2026-10-06

Every finding was addressed on the same branch; each fix is in a commit that names its findings,
and each confirmed defect that a test can reproduce has a regression test that fails without the
fix (re-checked by reverting the fix for F5 and F9).

| Finding | Resolution | Regression evidence |
|---|---|---|
| F1, F22 | Probes read an adapter's `candidate_too_large` as a cost that does not fit and keep any other code; probes use a notice at least as long as the final one. The test double now refuses oversized candidates exactly as the adapters do. | `automatic_fitting_hides_old_fragments_and_keeps_every_user_message`, `a_protected_floor_over_the_budget_refuses_before_any_request` (with the refusing double) |
| F2, F6 | Session, checkpoint and archive writes flush before they sync; the runner verifies each finalized object before publishing it and publishes `unavailable` for one that does not verify. | None automated: a short final write needs a process-wide file-size limit; recorded in `docs/context-management.md` |
| F3 | A search reads and seals its cursors at the snapshot its first page read. | `paging_a_search_reads_its_own_snapshot_after_the_session_grows` |
| F4 | The link trusts a reconciliation only for a file that is byte for byte a prefix of the held session. | `a_reconciled_file_must_be_a_prefix_of_the_held_session` |
| F5 | The inspect cursor binds revision and profile and positions by the last fragment listed. | `an_inspect_cursor_pages_the_whole_catalog_across_steps` (fails on the previous code) |
| F7 | Only activation upgrades a body; the CLI refuses a reserve flag without managed mode and recovers an open turn before any policy change. | `a_legacy_policy_change_leaves_a_version_two_body_alone` |
| F8 | OpenAI managed support is limited to the offered models, whose ceiling is declared. | `every_responses_route_is_unsupported_and_refused_before_any_request` (now also refuses `gpt-5`) |
| F9 | A window shorter than the needle resumes where it started. | `a_window_shorter_than_the_needle_resumes_where_it_started` (fails on the previous code) |
| F10 | Optional usage categories are absent rather than zero; raw counters are kept. | `a_counter_that_may_be_missing_is_never_written_as_zero` |
| F11 | A proposal staged in a failed step is refused. | `a_proposal_in_a_failed_step_is_never_accepted` |
| F12 | An intent refused at record admission is finished as `refused`. | Traced; covered by the attempt-uniqueness fold check |
| F13 | A failed recovery reports its actual persistence state to the host. | Traced |
| F14 | Kept as specified: CM-09 extends `read` and `grep` for every session. The documentation's legacy claim now names the exception: legacy requests are unchanged apart from those two schemas, whose results are unchanged when the new arguments are absent. | `a_call_without_byte_arguments_is_the_line_window_it_always_was` |
| F15 | Generated data is identified by position in the compiler and every adapter. | Adapter grammar tests now send a labelled reply elsewhere |
| F16 | The hidden-id cap limits how far fitting goes, not whether it fits. | `the_hidden_cap_limits_how_far_fitting_goes_not_whether_it_fits` |
| F17 | Note claims and goal objectives render on one line. | `model_written_text_cannot_forge_a_line_of_generated_data` |
| F18 | Accepted decisions and attempt outcomes are each recorded once. | `a_decision_or_an_attempt_outcome_is_recorded_once` |
| F19 | `StorePort::collect_artifacts` and `nanus sessions collect`. | `sweeping_an_idle_sessions_archive_reports_what_it_reclaimed` |
| F20 | A quarantined session reports itself unready; its reset starts from the stored session through a fresh binding. | `a_quarantined_session_reports_itself_unready` |
| F21 | One staging budget covers the queue in front of the sinks and their write buffers. | Existing capture tests |
| Plausible: broker shared across sessions | Leases and finalizations are keyed by session through a task-local scope the runner sets. | `the_same_call_id_in_two_sessions_never_crosses` |
| Plausible: policy before recovery | Fixed with F7. | — |
| Plausible: unbounded arguments before a late name | Managed chat decoders bound an unnamed call at one record. | Existing decoder tests |
| Plausible: candidate prefix not checked | A checkpoint must extend the stored file: same-version prefix digest, or event equality across the v2→v3 upgrade. | `a_checkpoint_that_rewrites_stored_history_is_refused` |
| Plausible: quota released during an in-flight write | Accepted: the store measures partial files on disk, so the window is a transient undercount of in-flight bytes, bounded by the staging budget. | — |
