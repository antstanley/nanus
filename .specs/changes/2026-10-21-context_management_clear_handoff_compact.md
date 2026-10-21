# Change: Clear, handoff and compact — session-level context controls

**Status:** Draft · **Date:** 2026-10-21 · **Owner:** Ant Stanley · **Target:** domain/session records, managed compiler, bundle runner, link protocol, CLI, TUI, docs

Three capabilities extend [managed context](../../docs/context-management.md) from *inside a session* to *across sessions*:

1. **Clear** — start a new clean session: clear the TUI, stop the old session, and start a new one.
2. **Handoff & clear** — create a handoff document that is used to start a new session, then clear.
3. **Compact** — compact the existing session: drop duplicate information, discard conflicting information, and summarise; then place a marker in the session that nanus knows excludes everything before it from the context, and add the compacted session after the marker.

All three are human- or host-initiated. None is automatic, none runs unattended, and none deletes or rewrites an event: the raw session log stays the only authority and remains untouched, exactly as today's managed invariant requires.

## Baseline this builds on

- **Sessions** ([docs/sessions.md](../../docs/sessions.md)): a session is an append-only event log; a name is an alias; a writer claims a session; the agent holds sessions open and a client attaches by `Request::New` or `Request::Attach`.
- **Managed context** ([docs/context-management.md](../../docs/context-management.md)): fragments, frontiers, revisions and decisions; the step transaction; `derive_effective_context` compilation order; the protected floor (every user message); checkpoints and recovery; recall reading pre-hidden evidence; the v3/v4 body records (`context/mode`, `context/revision`, `context/decision`, `artifact/published`, `request/attempt`, `context/recovery`); error codes; what is and is not verified.
- **Link** ([crates/nanus-link/src/protocol.rs](../../crates/nanus-link/src/protocol.rs)): `PROTOCOL_VERSION = 11`; `Request::New { name }` already starts and attaches a new session; `Request::Interrupt` stops a running turn; `Frame::Refused`/`Done`/`Failed` already close flows.
- **Interface** ([docs/tui.md](../../docs/tui.md)): `/clear` today empties the *screen* transcript (interface-owned, agent-free, answered in recorded transcripts too); `Ctrl+L` means the same thing; `/context` mutations already use an explicit `y` confirmation after an impact warning.
- **Compaction precedent**: the roadmap's item 21 named `/compact` as a context-window policy; item 9 explicitly deferred it: "`/compact` is a policy for the context window, which is item 21".
- **Handoff precedent**: `docs/context-management.md` records "child-session handoff, which the contract specifies only for a future subagent feature, is not implemented".

## Capability 1 — Clear

### Semantics

A **clear** is "stop talking on this conversation, and start a fresh one":

- **Stops the old session.** Any running turn is interrupted (`Request::Interrupt`) and its ending `Done`/`Failed` frame is awaited before anything else moves. The old session is **not** deleted and its log is not touched: it stays durable, keeps its name and archive, and remains resumable by name or id. "Stop" means *no work continues on it from this client*; the agent may keep holding it idle (existing `MAX_HELD_SESSIONS` LRU eviction applies).
- **Clears the TUI.** The transcript is reset to empty and the draft/composer toggles are left alone (the exact behaviour today's screen-level clear already has).
- **Starts a new session.** The client attaches to a brand-new session (`Request::New`). The new session inherits the harness defaults exactly as any new session does today: legacy context unless `--context-mode managed` is given, no captured shell, an unnamed session unless the caller names it.

### Command and confirmation

- `/clear` is **promoted** from the screen-only command to the session-level control. Because it abandons the current view, it requires the same explicit `y` after an impact warning that `/context` mutations already require. The warning states the old session is stopped but not deleted, and names its id so a reader can resume it.
- The pure screen erasure remains available as `Ctrl+L` (live) and as the local `/clear` in a recorded transcript with no agent attached — there, no session exists and the command keeps its current meaning.
- `clear [name]` optionally names the new session; the usual name-claiming rules (taken names refused, one word, case-insensitive) apply untouched.

### Interface behaviour

- If a turn is running, the warning says it will be stopped, and the clear waits for the interrupted turn's ending frame before confirming the new session — never racing it.
- The old session id is shown in the confirmation and in a notice after the clear, so the transition is reversible by the reader.
- No link protocol change is needed for clear alone: `Interrupt` + `New` already exist. This spec's protocol changes are only for handoff seeding (Capability 2).

## Capability 2 — Handoff & clear

### Semantics

A **handoff** creates a document that summarizes the conversation, and that document is used to start the new session. It is the "clear" of Capability 1 with the new session *seeded* with the handoff; the old session records that it handed off, and where.

The handoff is **not** a transcript and is not a proof. It is a bounded, structured orientation document whose purpose is to let the new session continue the work without the old context. It has two parts:

1. **A deterministic header** — assembled by the harness, never by the model: source session id and frontier, the goal and its phase and revision, context mode/policy, model/effort, workspace, and the date. These are facts already in the log.
2. **A model-written narrative** — one bounded assistant response produced by a dedicated maintenance request (below), following a fixed instruction set:
   - keep every user constraint (the same rule the manual policy states for hidden history);
   - drop duplicate information (a fact stated twice appears once);
   - resolve conflicting information (state the resolution, and label it as the model's resolution, not as observed fact);
   - record decisions made, current state, open/unresolved items, and next steps;
   - cite evidence by source reference only where managed context can address one (event/artifact positions), never by quoted content;
   - stay within the bound, and never forge a tool call, a user instruction, or a harness-looking line.

### Flow

1. Human invokes `handoff [name]` (name optional: the new session's name).
2. The agent drafts the handoff: the deterministic header plus the maintenance request (below). A `Frame::HandoffDraft { text }` is sent to the client, which displays it.
3. The human confirms or cancels. Confirmation with `y` commits; `Esc` cancels with no session created and no record written.
4. On confirmation, atomically:
   - write a `context/handoff` record into the **old** session (source frontier, the new session id, the document digest, and that it was human-confirmed);
   - create the new session via `Request::New` with the handoff as its first `UserMessage` (visible in the new transcript, so the reader sees exactly what the work was handed off with);
   - attach, and clear the TUI as in Capability 1.

### Link protocol (v11 → v12)

- `Request::New { name }` gains an optional `handoff: Option<HandoffSeed>`. A peer that sends it is a v12 peer; `PROTOCOL_VERSION` moves to 12, and an older peer is refused before any request, exactly as today.
- `HandoffSeed` carries the confirmed document text (bounded), the source session id, and the source frontier — nothing else.
- New two-phase requests/frames:
  - `Request::HandoffDraft { name: Option<String> }` → `Frame::HandoffDraft { text }` (or `Frame::Refused` with a bounded reason).
  - `Request::HandoffCommit { name: Option<String> }` → completes the flow above; the new session is attached and `Frame::Attached` follows. A commit with no pending draft, or for a name that has since been taken, is refused.
- The draft is held server-side keyed by the connection; a draft expires when the connection detaches or another draft is requested. Drafts are never written to a session.

## Capability 3 — Compact

### Semantics

A **compact** cuts the *effective context* of the existing session down to a summary plus what came after the cut:

- It produces a **compacted session**: a bounded summary of what happened before the cut — duplicate information dropped, conflicting information resolved (and labelled as such), everything else summarised — written by a maintenance request under the same fixed instruction set as the handoff narrative.
- It places a **marker** in the session: a new `context/compact` record at the frontier when compaction runs. Nanus knows from the marker **not to include anything before it in the context** — not user messages, not fragments, not notes that predate it.
- The **compacted session is added after the marker**: the summary travels with the record and is rendered as the leading generated message of every later effective request.

The raw log is untouched: pre-marker events stay on disk, stay readable by recall (see below), and stay the authority. Nothing is deleted; compaction is a derived-view cut, exactly like hiding fragments is today.

### The marker and the record

New `SessionEvent::ContextCompact` (serialized as `context/compact`), payload `CompactionRecord`:

| Field | Meaning | Bound |
|---|---|---|
| `boundary_event_count` | Exclusive event count of the marker itself: events with sequence `< boundary` are outside the context from now on | must equal the record's own sequence |
| `prefix_blake3` | BLAKE3 of the stored header and the first `boundary` event lines (the byte-for-byte prefix rule the frontier already uses) | — |
| `summary` | The compacted session text, inline | 32 KiB encoded |
| `summary_blake3` | Digest of `summary` | — |
| `supersedes` | Sequence of the previous `context/compact` marker, when one exists | strictly increasing |
| `method` | `model_generated` (maintenance request) — the only method in this proposal | enum |
| `decisions_referenced` | Count of pre-boundary decisions folded into the summary | u32 |

Invariants (validated on write and on read, like every v3 record): the record exists only in a managed body; `boundary` is within the frontier; `supersedes` points at the latest earlier marker; the summary is non-empty, bounded, and contains no control characters or forged label lines (the `one_line` rendering rule applies to how it is *shown*, but the stored text itself must not contain newlines that could forge harness lines — refuse, do not sanitize).

A session has at most one **active** marker — the newest. An older marker is itself before the boundary of the newer one, so its summary is replaced by the newer summary. Compaction nesting is therefore "latest wins", which is why `supersedes` is a chain rather than a set.

### Compiler rule

`derive_effective_context` and `derive_messages_indexed` change as follows:

- The selection starts from the active marker: the message fold keeps only events with `seq >= boundary`. The current "managed selection keeps every user message" assertion is scoped to the post-marker log: user messages *before* the marker are deliberately outside the context, which is the whole point of the cut and the one exception to the protected floor.
- The compacted summary is rendered as the **leading generated message**: an assistant-sourced, labelled memory message at position 0, before the first post-marker user message. The existing invariant "generated data is never the last message" is preserved and checked over the post-marker conversation.
- The system notice reports the cut: `compacted=true`, `compaction_boundary=<seq>`, and the summary digest, so the model can tell that the earliest content it sees is a summary rather than the original log.
- Fragments, notes, protection and hard fitting operate only over the post-marker portion. A hidden fragment id that predates the boundary is invalid (there is nothing to hide — it is already outside); a note that predates it is not carried into the effective request.
- Hard fitting may still hide post-marker fragments if the summary plus the continuation still outgrow the allowance; the protected floor resumes at the first post-marker user message.

### Recall

Recall keeps reading pre-marker evidence. The transcript and archives are unchanged; a `context_recall` search may still land in pre-marker events and artifacts, and a note may still cite them. The spec makes the split explicit in the interface: a recall hit before the boundary is labelled `pre_compaction`, and a note that cites one is allowed but rendered with that label. This is what keeps compaction from destroying evidence while still making the *context* small.

### The step transaction

Compaction is a managed operation and goes through the existing step transaction, with two human checkpoints:

1. **Draft** — `Request::CompactDraft` starts a maintenance step. The runner records the usual `request/attempt` intent (a `maintenance` attempt kind recorded on the existing attempt record), prepares a managed request over the *uncut* selection (everything before the proposed boundary is visible to the summary writer), and the model returns the summary. A `Frame::CompactDraft { text, draft_id }` is sent to the client. No record is written yet; the draft is held like the handoff draft.
2. **Preview** — the client shows the summary.
3. **Commit** — `Request::CompactCommit { draft_id }` with a confirmed `y`:
   - appends the `context/compact` record at the current frontier;
   - appends an accepted `context/revision` whose hidden set covers the post-marker selection (the boundary is carried by the marker; the revision records what is hidden *after* it) with `author=host`/`reason=compact`;
   - checkpoints both with the existing settle machinery, so the marker is durable before any later request is prepared under it;
   - the `context/decision accepted` record accompanies the revision, so the decision ledger stays complete.
   A cancel, a second draft, or a failed validation discards the draft and writes nothing. A draft whose frontier changed since it was made is refused as stale (`stale_base`), and the human is invited to draft again.

The maintenance request follows the same provider rules as any managed request: it is prepared and frozen by the adapter, it is refused (not clamped) if it cannot fit, it records usage, and it never calls a tool. It carries no user turn and no assistant reply that survives into the conversation — it exists only to produce the summary, so the summary is captured from the model's *text* output and validated before it is shown.

## Limits

Additions to the policy-1 limits table (`nanus_domain::context::managed::limits`):

| Quantity | Bound |
|---|---|
| Handoff document (header + narrative) | 32 KiB encoded; narrative ≤ 16 KiB |
| Compaction summary | 32 KiB encoded |
| Active compaction markers | 1; `supersedes` chain bounded by the session's existing record limits |
| Drafts (handoff/compact) held per connection | 1 each, expired on detach or replacement |
| `handoff`/`compact` maintenance requests | One per human confirmation; every model call charges the step budget like any managed step |

The `context/compact` and `context/handoff` records count against the existing 4 MiB per-record and 64 MiB per-session bounds.

## Error codes

New stable codes: `invalid_compact` (a malformed or out-of-order marker), `stale_draft` (a commit whose draft frontier moved), `handoff_unavailable` (no session to hand off from, or a refused name). All are refused before anything is mutated, carry bounded ids and counts, and never include source excerpts — the existing error-code rules.

## Scope and files

| Package | Where | Change |
|---|---|---|
| Domain, session log | `crates/nanus-domain/src/session.rs` | `SessionEvent::ContextCompact`, `SessionEvent::ContextHandoff`; extend `is_managed_record`; validation in `validate_event_replay`; `derive_messages_indexed` takes the active boundary |
| Domain, managed records | `crates/nanus-domain/src/context/managed/records.rs` | `CompactionRecord`, `HandoffRecord`, `HandoffSeed`, `MaintenanceAttemptKind`; `validate` for each; schema additions |
| Domain, managed state | `crates/nanus-domain/src/context/managed/state.rs` | `fold_strict` uniqueness for markers and handoff records; revision/boundary coupling |
| Domain, managed compiler | `crates/nanus-domain/src/context/managed/compile.rs` | boundary cut; leading summary message; notice fields; scoped protected-floor assertions |
| Domain, managed limits | `crates/nanus-domain/src/context/managed/limits.rs` | table above |
| Bundle, runner | `crates/nanus-bundle/src/agent_loop/managed/` | maintenance-step preparation, draft/commit staging, attempt records, settle integration |
| Link | `crates/nanus-link/src/protocol.rs` | `PROTOCOL_VERSION = 12`; `Request::New.handoff`; `HandoffDraft`/`HandoffCommit`, `CompactDraft`/`CompactCommit`; frames |
| CLI | `crates/nanus-cli/src/cli.rs` | `nanus run --handoff <ref>`, `--compact` flags for one-shot use; flags refused on a busy session |
| TUI | `crates/nanus-tui/src/{command,runtime,view,dialog}.rs` | `/clear` promotion (with confirmation), `/handoff`, `/compact`; draft preview dialogs; `Ctrl+L` unchanged |
| Docs | `docs/context-management.md`, `docs/sessions.md`, `docs/tui.md`, `docs/features.md`, `docs/roadmap.md` | add/modify blocks per the canonical-page convention |

## Docs changes (summary of canonical Add/Modify blocks)

- **`docs/context-management.md`** — add a "Clear, handoff and compact" section after [Human controls and clients](../../docs/context-management.md#human-controls-and-clients): the three capabilities, the marker rule, the protected-floor exception, recall labelling, the maintenance requests, and the "what is verified" additions. Modify the verification section's "child-session handoff … not implemented" line to point at this spec.
- **`docs/sessions.md`** — "What a session is": the two new records and the handoff-seeded first message; "What travels over the link": v12 requests/frames.
- **`docs/tui.md`** — command table: `/clear` new meaning, `/handoff`, `/compact`, and the confirmation wording; note `Ctrl+L` stays the screen erasure.
- **`docs/features.md`** — slash-command list.
- **`docs/roadmap.md`** — mark the item-9 `/compact` deferral and the item-21 policy as landed by this change.

## Acceptance evidence

Deterministic tests against the real runner, store, link and interface (macOS), at least:

- **Clear.** `/clear` with a running turn interrupts, waits for the ending frame, then attaches a new empty session; the TUI transcript is empty; the old session still loads by id with its log intact; confirmation is required (`Esc` cancels). A busy clearance never races the turn.
- **Handoff.** A confirmed `/handoff` writes `context/handoff` into the old session, creates the new session with the document as its first `UserMessage`, and attaches; the name-claiming rules hold; a stale or refused name commits nothing; `Request::New` with `handoff` from a v11 peer is refused (protocol mismatch); a draft is dropped on detach.
- **Compact.** A marker at the frontier: the compiler excludes every pre-boundary event, the summary is the leading generated message and is never last, the notice reports `compacted=true`, and the protected-floor assertion counts only post-marker user messages. A second compaction supersedes the first. A malformed marker (`boundary` ≠ its sequence, out-of-order `supersedes`, oversized/unbounded summary, forged label line) is refused on write and on read. Recall still finds pre-boundary hits and labels them `pre_compaction`. The draft → preview → commit flow refuses a stale draft, writes nothing on cancel, and checkpoints the marker + revision + decision together before any later request is prepared.
- **Legacy isolation (T01 parity).** A non-managed body carrying `context/compact` or `context/handoff` is refused; legacy requests are byte-identical apart from the existing documented schema exception.
- **Attempts/accounting.** A maintenance request records intent before HTTP, usage, and a finished attempt; a refused maintenance request is `refused`, never clamped.

Not verified, and not claimed (consistent with the existing contract): **no live provider has sent a maintenance request** (fixtures only); no quality/cost evaluation of compaction or handoff has been run; power-loss durability is not claimed; native Windows is cross-target lint only.

## Deviations and open questions

- **`/clear` naming.** This spec promotes the shipped `/clear` to the session-level control and keeps the pure screen erasure on `Ctrl+L`. The alternative — keep `/clear` as screen-only and add `/new` — is a one-word change to the command table and tests. The owner should confirm which spelling the shipped interface uses.
- **Handoff destination.** The handoff travels as the new session's first `UserMessage`. An alternative (writing `HANDOFF.md` into the workspace) is a filesystem side effect and is deliberately excluded; it can be added later if wanted.
- **Compaction reversibility.** The cut is forward-only in the effective context; the raw log and recall are the escape hatch. A future "un-compact" would need a revision that ignores the marker, which this spec leaves out.
- **Cost guardrails.** Drafts spend a model call before any human sees them. This is deliberate and matches the "preview before commit" pattern, but the draft request should be refused when the session is already over its turn budget, and the cost hazard text in `docs/design.md` should name the two maintenance requests.
