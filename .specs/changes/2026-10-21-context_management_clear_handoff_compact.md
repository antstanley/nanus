# Change: Clear, handoff and compact — session-level context controls

**Status:** Implemented (uncommitted) — the code and the docs follow this document; where they
differ on purpose, the difference is listed under *Deviations and open questions*, as built. A
review of this document against the code (2026-10-21) found gaps in recovery, the link's draft
reservation and interrupts, the text rule, frontier checks across a body-version move, and the
interface's state after a switch; they are fixed, and the behaviour they settle is written into the
sections below. ·
**Date:** 2026-10-21 · **Owner:** Ant Stanley · **Target:** domain/session records, legacy context
fitting, managed compiler, bundle runner, link protocol, TUI, docs

Three capabilities give a human control over a conversation's context *across* a session's
lifetime, not only inside one request:

1. **Clear** — start a new clean session: clear the TUI, stop the old session, and start a new one.
   Works in either context mode.
2. **Handoff & clear** — create a handoff document that seeds a new session, then clear.
   **Managed context only.**
3. **Compact** — compact the existing session: drop duplicate information, resolve conflicting
   information, and summarise; then place a marker in the session that excludes everything before
   it from the context, with the summary standing in for it. **Legacy context only.**

The split is deliberate. Handoff needs what managed context already has and legacy context does
not: request attempts with intent before HTTP, checkpoints, and a generated-data slot that is
labelled as data rather than as anyone's instruction. Compaction is the legacy mode's missing
lever. A managed session already shrinks its context with hiding, notes and hard fitting, and a
second, overlapping cut there would need a new exception to the protected floor. So the two
features never meet: a compacted session cannot become managed (below), and a session that is
not managed cannot hand off.

All three are started by a human at the interface. None is automatic, none runs unattended, and
none deletes or rewrites an event: the raw session log stays the only authority and its existing
events are never touched.

## Baseline this builds on

- **Sessions** ([docs/sessions.md](../../docs/sessions.md)): a session is an append-only event log;
  a name is an alias; a writer claims a session; the agent holds sessions open and a client
  attaches by `Request::New` or `Request::Attach`. `serve_connection` already releases the watched
  session and attaches the new one when an attached connection sends `New`.
- **Legacy context fitting** (`nanus_domain::context::fit`, roadmap item 21, shipped): prompt
  assembly replays the whole log; every message before the first user message is the harness's
  leading prompt and is never dropped; whole turns are dropped oldest first; the gap is announced
  by a **system message placed after the leading prompt**; a prompt whose newest turn cannot fit is
  refused (`context_budget`).
- **Managed context** ([docs/context-management.md](../../docs/context-management.md)): fragments,
  frontiers, revisions and decisions; the step transaction; the protected floor (every user
  message); checkpoints and recovery; the v3/v4 body records; the adapters' managed role grammar —
  leading system messages only, the generated message *directly after the first user message* and
  never last, every surviving call paired with exactly one result.
- **Body versions** (`nanus_domain::session`): a body is written at the lowest version that holds
  it — 2 legacy, 3 managed, 4 typed user content — and readers accept 1 to 4
  (`reads_session_version`). A managed record in a body below 3 is refused, so a fact cannot be
  smuggled into a log a reader treats as an older format.
- **Link** ([crates/nanus-link/src/protocol.rs](../../crates/nanus-link/src/protocol.rs)):
  `PROTOCOL_VERSION = 11`, checked at the handshake before any request; `Request::Interrupt` sets
  the running turn's stop flag, and the turn ends with `Frame::Done` carrying
  `TurnEnd::Interrupted`.
- **Interface** ([docs/tui.md](../../docs/tui.md)): `/clear` today empties the *screen* transcript
  (interface-owned, answered in recorded transcripts too) and ignores any words after it
  (`/clear now` is `Clear`); `Ctrl+L` does the same; `/context` mutations use an explicit `y` after
  an impact warning.
- **Roadmap item 9** deferred `/compact` to item 21. Item 21 shipped as the whole-turn budget
  policy without it.

## Capability 1 — Clear

### Semantics

A **clear** is "stop talking on this conversation, and start a fresh one":

- **Stops the old session.** A running turn is interrupted (`Request::Interrupt`) and its ending
  `Done`/`Failed` frame is awaited before anything else moves. The old session is **not** deleted
  and its log is not touched: it stays durable, keeps its name, and remains resumable by name or
  id. "Stop" means *no work continues on it from this client*; the agent may keep holding it idle
  (the existing held-session eviction applies).
- **Clears the TUI.** The transcript is reset to empty; the draft and composer toggles are left
  alone, as the screen-level clear does today.
- **Starts a new session.** The client sends `Request::New { name: None }`. The new session gets
  the harness defaults exactly as any new session does today: legacy context unless the agent was
  started with managed context, no captured shell, unnamed.

### Interrupting a turn that is waiting on an approval

Today a turn waiting on an approval question awaits only the answer (`server.rs`,
`LinkApprover::decide`), not the stop flag, so an interrupt does not end it. If another viewer
holds the question, a clear that waits for the ending frame would wait for that viewer. This
change closes the gap on the server, with no new frame:

- `Request::Interrupt` also answers every approval question the running turn has outstanding as
  `ApprovalOutcome::Unavailable`. That is fail-closed, matching what happens when nobody can
  answer, and it ends the wait.
- Every viewer is told the question is gone the same way a question answered elsewhere is today.

A tool already running is not killed by this change; it finishes or observes the stop the way it
does today. While the clear waits, the interface shows *stopping…*, and `Esc` abandons the
clear — the old session stays attached and nothing is created. The wait is therefore bounded by a
human, not by a timer.

### Command and confirmation

- `/clear` is **promoted** from the screen-only command to the session-level control. Because it
  leaves the current conversation, it requires the same explicit `y` after an impact warning that
  `/context` mutations already require. The warning says the old session is stopped, not deleted,
  and gives its id (and name, when it has one) so a reader can resume it.
- `/clear` still takes no argument, and words after it are still ignored (`/clear now` stays
  `Clear`). The new session is unnamed; naming stays where it is today. This keeps the existing
  parse and its test, with only the routing changed.
- The pure screen erasure stays on `Ctrl+L` (live), and `/clear` keeps its current screen-only
  meaning in a recorded transcript with no agent attached, where there is no session to stop.

### Interface behaviour

- If a turn is running, the warning says it will be stopped, and the clear waits for that turn's
  ending frame before sending `New`, so it never races the turn.
- Prompts queued for the conversation being left are discarded when the clear is confirmed, with
  a notice saying how many were not sent. They were written for the old conversation, so sending
  them into the new one would make its first turn one nobody asked of it.
- The warning takes nothing but `Esc` until the keyboard has been quiet for `APPROVAL_SETTLE`
  with it on screen, as an approval does, so a `y` that was being typed when the warning appeared
  never confirms it.
- The old session's id is shown in the confirmation and in a notice after the clear, so the reader
  can always get back to it. Its name is shown too, only when it has one.
- What belongs to a session follows the attachment, not the screen: the title, the token count,
  the goal and context lines are the new session's, and so is the context mode — managed after a
  handoff, and after a clear the agent's own answer, which the interface asks for at once.
- A prompt typed while a draft runs or while a new conversation is being attached is queued, not
  sent, and a committed handoff discards the prompts queued for the old conversation as a clear
  does. `/clear`, `/handoff` and `/compact` are refused with a notice while another is under way,
  so an attachment is never taken for the wrong switch. If the warning is open when a turn starts,
  it changes to say the turn will be stopped and asks for the keyboard's quiet again.
- The agent lets go of the conversation a connection follows before it opens the next one, so a
  `New` it cannot carry out leaves the connection following none. The interface says so, and names
  the old id and the `nanus tui --resume` that opens it again.
- No link vocabulary change is needed for clear: `Interrupt` and `New` already exist. The
  approval change above is server behaviour, not vocabulary.

## Capability 2 — Handoff & clear (managed context only)

### Availability

A handoff is offered only when the source session's current mode is **managed**. In a legacy
session, a recorded transcript, or with no session attached, `/handoff` is refused with
`handoff_unavailable` and nothing is written. Over the link, a handoff or compaction request on a
connection with no session is answered with `Frame::Refused` carrying `handoff_unavailable` or
`compact_unavailable`, not with the `Failed` frame other unattached requests get, because it is a
refusal of the request the client made. A busy session (a turn or another maintenance request
running) is refused the same way; the interface invites the human to wait or interrupt.

### Semantics

A **handoff** writes a document that summarises the conversation and seeds a new **managed**
session with it, then clears as in Capability 1. The old session records that it handed off, and
where; the new session records where it came from.

The handoff is **not** a transcript and is not a proof. It is a bounded, structured orientation
document whose purpose is to let the new session continue the work without the old context. It has
two parts:

1. **A deterministic header**, assembled by the harness and never by the model: source session id
   and frontier, the goal with its phase and revision, context mode and policy, model and effort,
   workspace, and the date. These are facts already in the log.
2. **A model-written narrative** — one bounded response from a dedicated maintenance request
   ([Maintenance requests](#maintenance-requests)), following the fixed instruction set:
   - keep every user constraint (the rule the manual policy states for hidden history);
   - drop duplicate information (a fact stated twice appears once);
   - resolve conflicting information (state the resolution, and label it as the model's
     resolution, not as observed fact);
   - record decisions made, current state, open items, and next steps;
   - cite evidence by source reference only where managed context can address one (event and
     artifact positions), never by quoted content;
   - stay within the bound, and never write a tool call, a user instruction, or a line in the
     harness's label grammar.

   The narrative is validated by the [text rule](#the-text-rule) before it is shown; a narrative
   that breaks it is refused, not repaired.

### How the new session carries it

The handoff document is **generated data, not a user message**. The model wrote the narrative,
and the narrative may restate content that came from tool output; putting it in a `UserMessage`
would give it the authority of the human and put it under the protected floor for good. It
travels instead as a managed record and is rendered in the slot managed context already has for
labelled generated data:

- The new session is created managed. Its first record is a `context/mode` record copying the
  source session's mode and policy (output reserve and shell-evidence capture included), so a
  handoff can neither turn capture on nor off. Its second record is `context/seed`
  (`SessionEvent::ContextSeed`, payload `HandoffSeed`).
- The managed compiler renders the seed as a **leading system message**, placed after the
  fixed-schema notice on every request, the first included: `Effective::seed`, built by
  `seed_message`. It opens with a fixed label (`HANDOFF_SEED_LABEL`) saying that the session was
  started from a handoff a model wrote from an earlier session, that what follows is generated
  data and not a user instruction and not proof, and that the earlier history is not part of this
  conversation; then `Source session: <id>.`, a blank line, and the document. It is not a
  fragment, so it cannot be hidden. It is composed into every candidate the fitter probes, so its
  bytes count toward the request's fit.
- A leading system message is the one slot every request has. The generated assistant message
  sits after the first user message and is never last, so the first request of a seeded session
  — one user message and nothing after it — has no generated message at all; a seed placed there
  would be missing from exactly the request that most needs it. A system message carries the
  harness's voice, so the label says who wrote the text, and the text rule refuses anything that
  looks like the harness's own lines. This is the same trade-off a compacted legacy prompt makes
  for its summary, and `SAFETY.md` records it.
- Until the human sends the first prompt, there is no request and nothing to render. The new
  session's first `UserMessage` is the human's own prompt.
- A seeded session switched to legacy context keeps its seed: legacy prompt assembly
  (`derive_prompt_messages`) places the same seed message directly after the system prompt, where
  the legacy fit's leading prompt keeps it. Without this the switch would silently leave the session
  with nothing it was started from. Once such a session is compacted, the summary stands in for the
  seed, since the compaction request was given it. A session with no seed assembles what it did.

`HandoffSeed` fields: `text` (the confirmed document, header plus narrative), `document_blake3`,
`source_session_id`, `source_frontier`. It is validated on write and on read like every managed
record: the text is a document — seven header lines opening with `HANDOFF_HEADER_FIELDS` in order,
a blank line, then the narrative — the whole within 32 KiB, the narrative within 16 KiB and not
imitating a header line, both under the text rule; the digest is truthful; and the frontier names
the source session. It may appear only as the session's second event, directly after the managed
`context/mode` record that opens it; the placement is checked when the session is written and when
it is read, not only when managed state is folded. A `context/handoff` back-link's
`source_frontier` must name the session that holds it.

### Flow

1. The human enters `/handoff [name]`. The optional single word names the new session. The
   interface refuses more than one word with a notice. The agent checks the name before any model
   call: the store's name rules (`nanus_ports::session_name_problem`: not blank, at most 128
   characters, no control character), one word, and not taken (case-insensitive).
2. The client sends `Request::HandoffDraft { name }`. The agent runs the draft in a task of its own,
   as it runs a turn, so the connection keeps reading. The draft reserves the session (it is busy
   for the duration, so no turn can start), closes anything a crash left open (below), runs the
   maintenance request, validates the narrative, assembles the header, and answers
   `Frame::HandoffDraft { draft_id, text }` — or `Frame::Refused` with a bounded reason. The
   maintenance request writes its `request/attempt` records (intent before HTTP, finished after).
   **These are the only records a draft writes**, and every viewer of the source is sent a
   `Frame::Checkpoint` for them, as for any idle checkpoint. The session is reserved before the
   request is answered, in the connection's own loop, so no turn can start in between and an
   interrupt read straight after the request finds the draft. `Request::Interrupt` from that
   connection — the interface sends it on `Esc` while a draft runs — stops the draft inside its
   model call: the attempt is recorded `cancelled` and the draft is refused as
   `handoff_unavailable`; one that arrives before the call has started refuses the draft before
   the model is asked. Only the connection that asked can stop a draft: another viewer's interrupt
   reaches a turn, never someone else's draft, and stopping a draft answers no approval. "Closes
   anything a crash left open" means a turn with no end as well as a dangling intent; it is done
   before the draft checks for an open turn. A checkpoint whose outcome is unknown quarantines the
   session, as a mode change's does.
3. The draft records the frontier *after* its own finished attempt record as its base. The
   interface shows the document read-only, with the same scroll keys the context warning uses.
4. The human confirms with `y` or cancels with `Esc`. A cancel sends nothing. The draft's attempt
   records stay, as accounting of a model call that was made; no handoff record and no session are
   written.
5. On confirmation, the client sends `Request::HandoffCommit { draft_id }` (below).

### The commit, in order

A commit touches two sessions, and the store has no transaction across sessions, so the steps are
ordered to leave the more useful half behind if a step fails:

1. **Check.** A pending draft exists for this connection with this `draft_id`; the source session
   is idle and not quarantined (else `handoff_unavailable`); its frontier still equals the draft's
   base (else `stale_draft`); the name, if any, is still free. The agent reserves the source session
   for the rest of the commit, so it cannot move underneath.
2. **Create the new session** with its `context/mode` and `context/seed` records, name it when
   asked, and checkpoint it through its `SessionCheckpoint`. If this fails, nothing exists and the
   commit is refused.
3. **Record the back-link**: checkpoint a `context/handoff` record into the old session —
   `source_frontier` (the draft base), `new_session_id`, `document_blake3`, and `confirmed: true`.
   The source's viewers, except the committing connection, which is leaving it, are sent a
   `Frame::Checkpoint` for it.
4. **Attach** the connection to the new session (`Frame::Attached`, `Frame::Backlog`, then
   `Frame::Seeded`), release the source session's reservation, and clear the TUI as in
   Capability 1. The new session is reserved from the moment it is held until this attachment, so
   it cannot be evicted as an idle, unwatched session in between.

If step 3 fails, the new session is complete and its `context/seed` names where it came from, so
the work is not lost. The agent still attaches the client, and `Frame::Seeded` says
`back_link_recorded: false`; the interface reports that the old session does not record the
handoff. The provenance the new session holds is authoritative; the back-link is a convenience
for a reader of the old session. `back_link_recorded` is read from the source session's log — a
`context/handoff` naming the new session — on every attachment, so it is the same answer after a
restart or from another agent; a source that cannot be read reads `false`.

A checkpoint whose outcome is unknown, in step 2 or step 3, is reconciled against the disk and
never written again: the seeded session is kept when the store holds it and let go when it holds
the previous file; the back-link counts as recorded only when the store holds it; and a session
whose state cannot be established is quarantined.

A session may hand off more than once (for example, after it is resumed). Each `context/handoff`
record names a distinct `new_session_id`, and their `source_frontier` event counts strictly
increase.

As built, the handoff's name is written after the session it names exists, so a name that fails at
that step does not undo the handoff. The session is complete and holds no name, `Frame::Seeded.name`
says so, and the interface shows a notice that it was created but not named. Two handoff drafts of
one session invalidate each other, because each draft's attempt records move the frontier past the
other's base: the first commit wins and the other is `stale_draft`. A draft made after another
draft's commit is the next one that can win.

## Capability 3 — Compact (legacy context only)

### Availability

Compaction is offered only when the session's current mode is **legacy**. A session that is
managed, busy, or a recorded transcript is refused with `compact_unavailable`. So is a session
whose log has no completed turn after the active marker, since there would be nothing to compact.
A session that was managed earlier and has since been switched back to legacy may be compacted,
even when the switch is the last thing in its log. `Session::can_compact` answers availability
with the same rule the write and the read check (below), so a draft is never paid for that its
commit would refuse.

Enabling managed context on a session that has a compaction marker is **refused** with
`compacted_session`, from every entry point (`nanus run --context-mode managed`, the link's
`Request::Context`, the interface). Managed selection keeps every user message, which is exactly
what a cut removes, and reconciling the two is out of scope. A human who wants managed context
after compacting starts a new session.

### Semantics

A **compact** cuts a legacy session's prompt down to a summary plus what follows the cut:

- A maintenance request writes a **summary** of the conversation up to the cut — duplicate
  information dropped, conflicting information resolved (and labelled as the model's resolution),
  everything else summarised — under the same fixed instruction set as the handoff narrative,
  except the evidence-citation rule, since a legacy session has no addressable fragments.
- A **marker** — a `context/compact` record — is appended at the cut. From then on, legacy prompt
  assembly folds only the events *after* the marker.
- The summary travels in the marker and is rendered as a **leading system message** on every
  later request, in the place the legacy drop notice already uses.

The raw log is untouched: events before the marker stay on disk and in the transcript, and stay
the authority. Nothing is deleted. A legacy session has no recall tool, so after a compaction the
model **cannot** read pre-marker content back, except what the summary carries and what is still
in the workspace. The confirmation warning says so in plain words; that is the price of the cut,
and the human pays it knowingly.

### Where the cut may fall

One rule says where a marker may sit, stated once (`CutState` in `nanus_domain::session`) and
asked by the availability check before a draft and by the write and read checks for every
marker. A cut is admitted when:

- **no turn is open**: every `turn/start` before it has its `turn/end`;
- **the mode in force is not managed**; and
- **a turn completed since the previous marker**, or since the start when there is none.

Events after the last turn that produce no message — a `context/mode` switch back to legacy, a
`goal/change` made between turns, a handoff's records — do not move it, so they do not stop a
cut. A tool call that a closed turn left unanswered (an interrupted turn's) does not stop a cut
either: its turn is over, so no result can follow it, and the fold already drops it. What is
refused is the thing a cut must never do, separate a call from the result that answers it: a
result after a marker that answers a call before it is `invalid_compact`. Because the event after
the marker that can produce a message is the next human prompt, the post-marker conversation
always opens with a user message, which is what the legacy fit's turn split expects.

### The marker and the record

New `SessionEvent::ContextCompact` (serialized as `context/compact`), payload `CompactionRecord`.
It is **not** a managed record: it is a legacy record that needs body version 5
([Body version](#body-version)).

| Field | Meaning | Bound |
|---|---|---|
| `boundary_event_count` | The marker's own sequence. Events with a lower sequence are outside the prompt from now on | Must equal the record's sequence |
| `prefix_blake3` | `Session::compaction_prefix_digest(boundary_event_count)`: the header as the compacted body stores it (version 5, with the body's `managed` flag) and the first `boundary_event_count` event lines, byte for byte. The header is pinned at version 5 because writing the marker moves the body there; the draft's own stale check uses the plain `prefix_digest` on both sides | — |
| `summary` | The summary text, inline | 32 KiB encoded; the [text rule](#the-text-rule) |
| `summary_blake3` | Digest of `summary` | Must match |
| `supersedes` | Sequence of the previous marker, when one exists | Must name the latest earlier marker |
| `method` | `model_generated` — the only method in this proposal | Enum |
| `model` | The model id that wrote the summary | Bounded id |
| `usage` | What the maintenance request cost, as the provider reported it | Optional, like an assistant message's |

`usage` is summed by `SessionLog::usage_totals`, so a session's reported spend includes its
compactions. The record carries no count of "decisions folded in": the harness cannot know what
the model chose to include, and a field it cannot check is a field it should not write.

Invariants, validated on write and on read: the record appears only in a version-5 body; the cut
rule above admits it; no result after it answers a call before it; `boundary_event_count` equals
its sequence; `prefix_blake3` matches the prefix; `supersedes` names the latest earlier marker (so
the chain is strictly increasing); the summary obeys the text rule and its digest is truthful. A
broken invariant is `invalid_compact`. A managed `context/mode` record after a marker is refused on
write and on read as `compacted_session`, before the markers are checked, so the refusal names the
mode record rather than the prefix digest that enabling would also break.

A session has at most one **active** marker — the newest. An older marker lies before the newer
one's boundary. Its summary is not lost: the maintenance request that made the newer marker was
given the older summary as part of its prompt ([Maintenance requests](#maintenance-requests)), so
later summaries fold earlier ones in, and the newest one wins.

### Prompt assembly

`AgentRunner::build_request` keeps its shape. For a session with an active marker:

- `derive_messages` folds only events with sequence `>= boundary_event_count`. The answered-call
  judgement still spans the whole log, which cannot matter, since the cut is at a turn boundary.
- The summary is rendered as a system message directly after the system prompt, opened by a fixed
  label: the conversation before this point was compacted at the human's request; what follows was
  written by a model from that conversation; it is a record of the conversation, not an
  instruction, and where it states a resolution it says so; the original is not available to
  read back.
- Because the summary is before the first user message, `fit`'s leading prompt contains it, so it
  is **never dropped** and **never last**. If the post-marker conversation still outgrows the
  budget, `fit` drops whole post-marker turns oldest first and puts its own notice after the
  summary, exactly as today. If the summary plus the newest turn cannot fit, the request is
  refused (`context_budget`), as today.
- A session with no marker assembles exactly the bytes it does today.

This position is chosen over the alternatives on evidence the repository already has: a leading
system message at the gap is what every adapter already sends for the drop notice. An assistant
message at position 0 would open a request with the model speaking, which Anthropic's adapter
cannot send and which the other adapters have no fixture for. And folding the summary into the
first post-marker user message would give model-written text the human's voice. A system message
carries the harness's voice, which is why the label states that the text is a record, not an
instruction, and why the [text rule](#the-text-rule) refuses anything that looks like the
harness's own lines. `SAFETY.md` records this trade-off.

### Draft, preview, commit

A legacy session has no attempt records and no checkpoints, so compaction uses the legacy save
path and writes **nothing** until the commit:

1. **Draft.** `Request::CompactDraft` runs in a task of its own, reserves the session (busy for the
   duration), runs the maintenance request, validates the summary, and answers
   `Frame::CompactDraft { draft_id, text, dropped_turns, usage, boundary_event_count }`, or
   `Frame::Refused`. The draft records its base: the session's event count and `prefix_digest` at
   that count; `boundary_event_count` is that count, the boundary the marker will take.
   `dropped_turns` is non-zero when the maintenance request itself had to be fitted. The summary
   then covers only the turns the model saw, and the preview says so. `Request::Interrupt` from
   that connection (`Esc` in the interface) stops the draft inside its model call, and it is
   refused as `compact_unavailable`.
2. **Preview.** The interface shows the summary, the event count it cuts at (or that the count is
   unknown, when the frame carries `0`), and the warning that earlier content leaves the
   model's reach. Like the handoff preview, it takes nothing but `Esc` until the keyboard has been
   quiet for `APPROVAL_SETTLE` with it on screen.
3. **Commit.** `Request::CompactCommit { draft_id }`, sent after an explicit `y`:
   - refused with `stale_draft` when the session's event count or prefix digest no longer equals
     the draft's base (a turn ran in between), and with `compact_unavailable` when the session is
     now busy or managed;
   - otherwise appends the `context/compact` record to a copy of the session and saves it the way
     an idle change is saved: a legacy body through `StorePort::save` (`try_to_jsonl`; a failed
     save leaves the file intact), and a body that still holds managed records — a session managed
     once and switched back to legacy — through its `SessionCheckpoint`, honouring the
     `PersistenceState` it returns (no second save after `Acknowledged`, nothing after `Unsaved`,
     only reconciliation after `Unknown`); the held session is replaced only once the store holds
     the cut. A cut the store did not save is refused with `checkpoint_not_committed` and the file
     is as it was; one whose outcome cannot be established is refused with `checkpoint_unknown`
     and the session is quarantined;
   - tells every viewer with `Frame::Compacted { boundary_event_count, summary }`.

A cancel, a second draft, a detach, or a failed validation discards the draft. Since a legacy
draft records nothing, a cancelled draft's spend appears only in its `CompactDraft` frame, not in
the session's totals. The cost-guard question below is about exactly this.

## Maintenance requests

Both drafts make one model call that is not a turn. It never reaches the conversation, and it
never runs a tool. The shape is the same in both modes:

```
system prompt                     (as the session's requests have it)
[previous compaction summary]     (legacy, when a marker exists)
conversation                      (legacy: post-marker messages, fitted by `fit`;
                                   managed: the effective selection the session's next request
                                   would carry, generated message in its slot)
user: <fixed maintenance instruction for handoff or compact>
```

- The last message is a harness-authored **user** message carrying the fixed instruction set. It
  is a valid final message in both role grammars: managed context allows only leading system
  messages, and a request must end with someone to answer. It is not recorded.
- Tool schemas are sent **unchanged**, so a history that contains tool calls stays valid on
  providers that require their definitions. A response that contains any tool call is refused and
  nothing is executed. Only the response's text is kept, then validated.
- **Managed (handoff):** the request goes through the managed step transaction like any other
  managed request: prepared and frozen by the adapter, refused (never clamped) if it cannot fit,
  intent recorded before HTTP and a finished record after, with `kind: handoff` on the attempt.
- **Legacy (compact):** the request is built by `build_request` and fitted by `fit`. It records
  nothing, and its usage travels in the draft frame and, on commit, in the marker.
- Neither request counts against a turn's step budget, because neither is a step of a turn. Each
  is one model call per human request, and a human confirms before its result is used.
- Both can be stopped. The runner takes a `TurnControl` (`draft_compaction_controlled`,
  `draft_handoff_controlled`), and a stop ends the model call at once, as it ends a turn's; a
  stopped draft is refused, never kept as a partial summary.
- A managed handoff first closes anything a crash left open, as a turn does. A handoff intent left
  with no outcome is closed on its own — finished `unknown_dispatch`, under a `context/recovery`
  of reason `settled_maintenance_attempt` — and never swept into an unrelated turn's recovery.
- A handoff draft's finished attempt carries its usage, and `usage_totals` and `usage_by_model`
  count it (under the attempt's model), so a session's reported spend includes its handoff drafts
  as it includes its compactions. A turn step's attempt is not counted: its assistant message
  already carries its usage.

## The text rule

Both the compaction summary and the handoff narrative are model text that the harness later
renders in its own voice or beside its own labels, so both pass one rule before they are shown,
and again on read:

- non-empty — text that is only whitespace is empty — and within its bound;
- valid UTF-8 with no control characters **except** `\n` and `\t`; `\r` and every other control
  character is refused;
- no invisible formatting character that could hide what a line says: the zero-width space, the
  word joiner and invisible operators, the byte-order mark, the soft hyphen, the Mongolian vowel
  separator, bidirectional marks, embeddings, overrides and isolates, interlinear annotation
  characters, and tag characters (`nanus_domain::context::is_invisible_format`). The zero-width
  joiner and non-joiner are allowed, because emoji and some scripts need them;
- no line that, after leading whitespace and any joiner, opens with the harness's label grammar
  (`[nanus`, in any case, with joiners inside the opening ignored too), so the text cannot forge a
  notice, a label, or a generated-data boundary;
- for the handoff narrative, no line that imitates the deterministic header's field lines; the
  seed's document is checked for this on read as well, by splitting it into its header and its
  narrative.

Newlines are allowed. A structured summary needs them, and refusing every multi-line response
would refuse nearly every real one. Markdown, including lines that open with `[` for a link, is
allowed; only the harness's own prefix is refused. Text that breaks the rule is **refused, never
sanitized**. The draft fails with `invalid_compact` (or `handoff_unavailable` for a narrative),
and the human may draft again. A stored summary or seed that breaks it is refused on read with
the same codes.

### As built: fitting, refusals and the instruction

- The fixed instruction is appended before the fit, so the fitter counts it. A compaction whose fit
  left no real turn beside the instruction is refused as `compact_unavailable`: a summary of nothing
  is not compacted. The fixed set for a handoff is the compaction set with the evidence-citation
  rule added.
- A request the adapter refuses before it is prepared writes no attempt record. There is no
  selection identity or request digest to record for a request that was never frozen, so the handoff
  is refused as `candidate_too_large`, and nothing is clamped.
- A handoff uses the accepted selection and does no automatic fit, so it never hides anything on its
  own behalf.
- A handoff whose whole document would pass its bound once the header is added is recorded
  `refused`, not `completed`: the header's length is known before the outcome is written.
- A reply that stopped at the model's output ceiling is refused, because a truncated summary or
  narrative is not one a reader can review as complete. A compaction is refused as
  `invalid_compact`; a handoff's attempt is recorded `refused`, as a narrative refusal is, and the
  draft is refused as `handoff_unavailable`.

## Body version

A maintenance request leaves records a version-4 reader does not know: `context/compact`,
`context/seed`, `context/handoff`, and a `kind` field on `request/attempt` (what a handoff draft
writes), which a strict version-4 parser refuses part-way through the body. An older build that
accepts versions 1 to 4 would otherwise open such a session and fail mid-way. This change therefore
adds **body version 5** for all of them:

- A body is written at version 5 when it carries at least one `context/compact`, `context/seed`
  or `context/handoff` record, or a `request/attempt` with a `kind`
  (`SessionEvent::needs_version_five`), and otherwise at the version it would be written at
  today. A session that never compacts or hands off never moves version, and its bytes are
  unchanged.
- Version 5 holds everything version 4 can, typed user content included. Its header's `managed`
  flag means what it means at version 4. A compacted session is legacy now, but it may carry
  managed records from an earlier managed period; a seeded or handing-off session is managed.
- Readers accept 1 to 5 (`NEWEST_SESSION_FORMAT_VERSION = 5`). Any of those records, or an attempt
  with a `kind`, in a body below version 5 is refused on read, and the write chooses version 5
  whenever the body holds one.
- An older build sees version 5 in the header, refuses the session through `reads_session_version`,
  and lists it as unreadable, instead of opening it and failing part-way through the body.
- `context/seed` and `context/handoff` are managed records as well, so the existing rule also
  confines them to managed bodies: version 5 marked managed.
- A frontier's digest covers the header, and the header names the version, so a body that moves to
  version 5 (or from 3 to 4) would otherwise orphan every frontier taken before the move — an
  accepted revision's base among them, which the next managed request checks. A frontier is
  therefore checked against the header the body has now and, failing that, against the header it
  had at each earlier version (`check_frontier`); a digest that matches none is stale, as before.

## Link protocol (v11 → v12)

New requests:

- `Request::HandoffDraft { name: Option<String> }` → `Frame::HandoffDraft { draft_id, text }` or
  `Frame::Refused`.
- `Request::HandoffCommit { draft_id }` → the commit above; then `Frame::Attached`,
  `Frame::Backlog`, and `Frame::Seeded`.
- `Request::CompactDraft` →
  `Frame::CompactDraft { draft_id, text, dropped_turns, usage, boundary_event_count }` or
  `Frame::Refused`.
- `Request::CompactCommit { draft_id }` → `Frame::Compacted` to every viewer, or `Frame::Refused`.

New frames:

- `Frame::Seeded { text, source_session_id, source_event_count, name, back_link_recorded }` is sent
  after `Backlog` to a client attaching to a seeded session, on every attachment, so the reader
  always sees what the session was started with. An interface that has already drawn the seed
  from the log it replayed draws it once, not twice.
- `Frame::Compacted { boundary_event_count, summary }` is sent live on commit. The attachment
  snapshot marks the same boundary for a client that attaches later, so the interface draws a
  divider at the cut with the summary beneath it, folded by the same `Ctrl+T` collapse as a long
  tool run, and keeps the pre-marker transcript visible above it.

`Request::New` does **not** change. The seed is held on the server from the draft it made, and
only `HandoffCommit` can use it, so a client cannot seed a session with text the agent never
drafted or claim a source it never had.

`PROTOCOL_VERSION` moves to 12 because an agent at 11 cannot parse the new requests, and the
handshake refuses a mismatched peer before any request is sent. The fields that may be absent are
defaulted, each with a doc comment saying what its absence means: `Refused.code`, `Seeded.name`,
`CompactDraft.usage`, `CompactDraft.boundary_event_count` (`0` is unknown) and
`StreamMark.compaction_boundary`. Since the handshake admits only a version-12 peer, the defaults
are for a field a v12 sender legitimately leaves out (no code, no name, no usage, no marker), not
for an older peer.

Drafts are held on the server, keyed by connection: at most one handoff draft and one compact draft
per connection. A draft is discarded when its connection detaches or attaches elsewhere (`New`,
`Attach`), when another draft of the same kind is requested, when it is committed, and, for every
draft the connection holds, when a handoff commit moves the connection to the new session. A draft
still running when its connection let go is dropped when it finishes. A `draft_id` that does not
match the held draft is refused as `stale_draft`. Two viewers of one session may each hold a draft.
A compaction draft writes nothing, so the first commit moves the session and the second is
`stale_draft`. A handoff draft writes its attempt records, so a draft made after another moves the
session past that other's base: whichever commit comes first wins, and the rest are `stale_draft`.

As built, a refusal of a draft or a commit, and a context mode change the runner refused with a code
(`compacted_session`), carries its stable code in a `code` field of `Frame::Refused`, which is
defaulted and omitted when absent. The sentence in `message` is the bare sentence: it never repeats
the code and never says "managed context" for a legacy compaction (a compaction refusal is its own
`BundleError::Compaction`), so a refusal that has no code reads as a bare sentence, as it did.
`Frame::CompactDraft.usage` is an `Option` of the draft's usage, because a provider may report none.
The stream mark carries `compaction_boundary` on `Attached` and `Backlog`. The interface does not
read it: its transcript is replayed from the log, where the marker is, so the divider is drawn from
the marker. The field stays for clients that have no store to read.

## Limits

Additions to the limits tables. Handoff limits sit beside the policy-1 managed limits
(`nanus_domain::context::managed::limits`); compaction limits sit beside the legacy context module.

| Quantity | Bound |
|---|---|
| Handoff document (header + narrative) | 32 KiB encoded; narrative ≤ 16 KiB |
| Compaction summary | 32 KiB encoded |
| Active compaction markers | 1, the newest; the `supersedes` chain is bounded by the session's record limits |
| Drafts held per connection | 1 handoff draft and 1 compact draft; discarded on detach, replacement or commit |
| Maintenance requests | One per human request; none is a step of a turn |

`context/compact`, `context/seed` and `context/handoff` count against the existing 4 MiB
per-record and 64 MiB per-session bounds.

## Error codes

New stable codes, all refused before anything is mutated, carrying bounded ids and counts and never
source excerpts, under the existing error-code rules:

| Code | When |
|---|---|
| `invalid_compact` | A marker that breaks an invariant, on write or on read; a summary that breaks the text rule |
| `compact_unavailable` | Compaction asked of a managed, busy or recorded session, of one with nothing after its marker, or with no session attached; a draft that was interrupted |
| `compacted_session` | Managed context asked of a session that has a compaction marker, from any entry point, and a managed mode record after a marker on write or read |
| `handoff_unavailable` | Handoff asked of a legacy, busy or recorded session, or with none attached; a narrative that breaks the text rule; a draft that was interrupted; a malformed seed or handoff record |
| `stale_draft` | A commit whose draft base moved, or whose `draft_id` is not the one held |

A taken name, or one that breaks the store's name rules, is refused with the existing name refusal,
not a new code. A compaction commit the store could not save is refused with the existing
`checkpoint_not_committed` or `checkpoint_unknown`. `stale_base` keeps its
current meaning inside the managed step transaction and is not used by a commit.

## Scope and files

| Package | Where | Change |
|---|---|---|
| Domain, session log | `crates/nanus-domain/src/session.rs` | `SessionEvent::{ContextCompact, ContextSeed, ContextHandoff}`; `ContextCompact` is not managed, the other two are; body version 5 for every maintenance record (`needs_version_five`) and `NEWEST_SESSION_FORMAT_VERSION = 5`; the one cut rule (`CutState`) shared by `can_compact` and the write and read checks; `compacted_session` on write and read; `derive_prompt_messages` starts at the active marker; `usage_totals` and `usage_by_model` sum marker and handoff-draft usage |
| Domain, legacy context | `crates/nanus-domain/src/context.rs`, `context/compact.rs` | Compaction limits; the summary's leading system message and label; the text rule; the record |
| Domain, managed records | `crates/nanus-domain/src/context/managed/records.rs` | `HandoffSeed`, `HandoffRecord`, `MaintenanceAttemptKind::Handoff`, `RecoveryReason::SettledMaintenanceAttempt`; the handoff document's header fields and split; validation on write and read; schema additions |
| Domain, managed state and compiler | `crates/nanus-domain/src/context/managed/{state,compile}.rs` | Seed placement (second event only), the handoff-record chain rule, the seed as a leading system message (`Effective::seed`, `seed_message`, `HANDOFF_SEED_LABEL`); recovery of a maintenance intent on its own; enabling managed refused on a compacted session |
| Domain, managed limits | `crates/nanus-domain/src/context/managed/limits.rs` | Handoff rows of the table above |
| Ports | `crates/nanus-ports/src/{store,context}.rs` | `session_name_problem`, the store's name rules, so a handoff checks a name before the model is asked; `CheckpointReason::Handoff` |
| Bundle, runner | `crates/nanus-bundle/src/agent_loop.rs`, `agent_loop/maintenance.rs`, `agent_loop/managed/` | Maintenance-request preparation in both modes, with a stop control; the seed composed into every managed candidate; the managed handoff attempt and its recovery; `BundleError::Compaction` and the bare `reason()`; mode-change refusal |
| Adapters | `crates/nanus-adapter-{anthropic,deepseek,openai}` | Fixtures only: a leading summary system message in a legacy request, and the seed as a leading system message in a managed request, the first included; no change to either role grammar |
| Link | `crates/nanus-link/src/{protocol,server}.rs`, `server/{maintenance,context}.rs` | `PROTOCOL_VERSION = 12`; the four requests and four frames; per-connection drafts run in their own task and stoppable; the ordered handoff commit with reconciled unknown outcomes; a compaction commit through the checkpoint for a managed body; `back_link_recorded` read from the source; `Interrupt` answering outstanding approvals as unavailable |
| TUI | `crates/nanus-tui/src/{command,runtime,view,dialog,transcript,maintenance,replay,help,layout,lib,runtime_accessibility}.rs` | `/clear` promotion with confirmation and the stopping wait; `/handoff [name]`, `/compact`; read-only draft previews behind the approval's settle delay; the seed block and compaction divider, live and in replay, drawn once; `Ctrl+L` unchanged |
| CLI | `crates/nanus-cli/tests/compacted_session.rs` | A test only: `nanus run --context-mode managed` on a compacted session is refused as `compacted_session` |
| Docs | See below | Add/modify blocks per the canonical-page convention |

The CLI gains no command in this change. `nanus run` cannot ask a human to confirm a draft, and a
handoff or compaction nobody confirmed is not one this spec allows. See the deferral below.

## Docs changes (summary of canonical Add/Modify blocks)

- **`docs/context-management.md`**: add a "Handoff" section after
  [Human controls and clients](../../docs/context-management.md#human-controls-and-clients) — the
  seed record, its rendering as a leading system message, the commit order, and the "what is
  verified" additions. State that compaction is legacy-only and that enabling managed context on a
  compacted session is refused. Leave the "child-session handoff … not implemented" line in place:
  it is about a future subagent contract, which this human-started handoff does not implement.
- **`docs/sessions.md`**: "What a session is" — body version 5, the marker, the seed and the
  back-link; "What travels over the link" — v12 requests and frames, per-connection drafts.
- **`docs/tui.md`**: the command table — `/clear`'s new meaning and confirmation, `/handoff`,
  `/compact`; the compaction divider; `Ctrl+L` stays the screen erasure.
- **`docs/features.md`**: the slash-command list; compaction under legacy context, handoff under
  managed context.
- **`docs/design.md`**: the cost hazard names both maintenance requests and says a draft spends a
  model call before anyone sees it, and that a handoff draft's cost is in the session's totals.
- **`docs/roadmap.md`**: amend item 9's "`/compact` … is item 21" to say `/compact` landed with
  this change for legacy sessions. Item 21 is already shipped and is not re-marked.
- **`SAFETY.md`**: the compaction summary and the handoff seed are model-written text sent in the
  harness's system voice after human confirmation, each labelled for what it is (a record; a
  handoff a model wrote) and filtered by the text rule; neither is ever a user message.
- **`AGENTS.md`**: the body-version invariant ("readers accept 1 to 5; version 5 when a body
  carries a maintenance record"), and the managed invariant gains "a compacted session cannot
  become managed". The managed floor itself is unchanged, because managed selection never sees a
  cut.

Done with this change: each page the table above names is updated as written. The differences from
it are listed under *Deviations and open questions*, as built.

## Acceptance evidence

Deterministic tests against the real runner, store, link and interface (macOS), at least the
following. Each claim is tested in both directions. The test that covers each item is named after
it (crate and file in brackets).

- **Clear.**
  - `/clear` with a running turn interrupts, waits for the ending frame, then attaches a new empty
    session; the TUI transcript is empty; the old session still loads by id with its log intact.
    Tests:
    `clear_interrupts_waits_for_the_ending_then_starts_a_new_session_and_the_old_one_is_untouched`
    [link]; `a_clear_during_a_turn_waits_for_its_ending_before_it_starts_the_new_conversation`,
    `the_attachment_after_a_clear_shows_the_new_conversation_and_names_the_old` [tui].
  - Confirmation is required, and `Esc` cancels both before the interrupt and during the wait; the
    warning takes only `Esc` until the keyboard is quiet. Tests:
    `clear_on_a_live_conversation_asks_first_and_escape_keeps_it`,
    `y_on_the_clear_warning_confirms_the_clear_of_that_conversation`,
    `escape_during_a_clear_wait_abandons_it_and_creates_nothing`,
    `a_confirmation_takes_only_escape_until_the_keyboard_has_been_quiet`,
    `the_clear_warning_names_a_session_only_when_it_has_a_name` [tui].
  - A confirmed clear never sends the old conversation's queued prompts into the new one.
    Tests: `a_clear_that_waited_on_a_turn_never_sends_its_queued_prompts_into_the_new_session`,
    `a_turns_ending_without_a_clear_still_sends_the_next_queued_prompt` [tui].
  - A turn waiting on an approval that another viewer holds ends on `Interrupt` with the approval
    answered as unavailable, so the call does not run.
    Tests: `an_interrupt_ends_a_turn_waiting_on_an_approval_another_viewer_holds`,
    `an_interrupt_answers_an_open_approval_as_unavailable_and_the_call_does_not_run` [link].
  - `/clear now` still parses as `Clear`, and a recorded transcript's `/clear` stays screen-only.
    Tests: `a_recorded_clear_stays_on_the_screen_and_clear_now_still_parses` [tui].
- **Handoff.**
  - A confirmed `/handoff` on a managed session writes the new session's `context/mode` and
    `context/seed` first, then `context/handoff` in the old session, and attaches. Tests:
    `a_handoff_commit_attaches_a_seeded_session_and_records_the_back_link` [link]; the order is
    shown by `a_failed_back_link_leaves_a_complete_seeded_session_and_says_so` [link], where the
    seeded session is complete although the back-link was never written.
  - The seed is a leading system message on every request of the new session, the first included,
    and never a `UserMessage` or part of the generated message; a session with no seed sends what it
    always did. Tests: `a_handoff_seed_is_a_leading_system_message_and_never_a_user_message`,
    `the_first_request_of_a_seeded_session_carries_its_seed` [domain compile tests];
    `the_first_request_of_a_seeded_session_leads_with_the_seed_as_a_system_message`,
    `a_managed_session_with_no_seed_sends_the_request_it_always_did` [bundle]; and in each adapter,
    `a_seeded_sessions_first_request_carries_the_seed_after_the_notice`,
    `a_seeded_sessions_later_request_still_leads_with_the_seed`,
    `a_managed_session_with_no_seed_sends_no_seed`.
  - A draft writes only its attempt records, and a cancel writes nothing more. Tests:
    `a_handoff_writes_only_its_intent_and_its_finished_attempt_both_of_kind_handoff` [bundle];
    `escape_on_a_handoff_preview_sends_nothing` [tui].
  - The commit is refused as `stale_draft` when the source moved, and with the name refusal when the
    name was taken in between; a name that breaks the store's rules or is more than one word is
    refused before the model is asked. Tests:
    `the_back_link_is_refused_as_stale_when_the_source_moved_after_the_draft` [bundle];
    `the_first_handoff_commit_wins_and_the_other_draft_is_stale`,
    `a_taken_handoff_name_is_refused_at_the_draft_and_again_at_the_commit`,
    `a_handoff_name_that_breaks_the_rules_is_refused_before_the_model_is_asked` [link].
  - A failed back-link checkpoint leaves a complete seeded session and `back_link_recorded: false`,
    on the commit and on a later attachment. Tests:
    `a_failed_back_link_leaves_a_complete_seeded_session_and_says_so` [link];
    `a_seed_reports_a_missing_back_link_and_a_name_that_did_not_stick` [tui].
  - `/handoff` on a legacy session is refused with `handoff_unavailable`, and its body is
    unchanged. Tests: `a_handoff_is_refused_on_a_legacy_session_and_writes_nothing` [bundle];
    `a_handoff_is_refused_on_a_legacy_session` [link].
  - A narrative with a `[nanus` line, a `\r`, an imitated header line, or an oversized length is
    refused, while a multi-line markdown narrative is accepted, and a stored seed is held to the
    same rule on read. Tests:
    `a_handoff_narrative_with_a_label_line_or_a_carriage_return_is_refused`,
    `a_handoff_narrative_that_imitates_a_header_field_is_refused`,
    `a_refused_handoff_request_is_recorded_as_refused_and_nothing_is_clamped` [bundle];
    `a_seed_whose_narrative_imitates_the_header_is_refused_on_read`,
    `a_seed_whose_narrative_is_over_its_bound_is_refused_and_one_at_it_is_not`,
    `a_seed_with_a_harness_label_line_is_refused_on_write`,
    `a_seed_without_the_harness_header_is_refused` [domain].
  - A draft is dropped on detach, and when the same connection attaches elsewhere.
    Tests: `a_handoff_draft_is_dropped_when_its_connection_lets_go`,
    `a_handoff_draft_is_dropped_when_the_same_connection_attaches_elsewhere` [link].
  - A second handoff from the same session is accepted.
    Tests: `two_handoffs_to_distinct_sessions_with_increasing_frontiers_are_accepted` [domain].
  - The seed is drawn once when a client attaches to a seeded session it replayed.
    Tests: `a_live_attach_to_a_seeded_session_draws_its_seed_once`,
    `the_attachment_a_handoff_makes_draws_its_seed_once` [tui].
- **Compact.**
  - A marker at a turn boundary: the next request folds only post-marker events, the summary is a
    leading system message that `fit` never drops, and a drop notice, when needed, follows it.
    Tests: `with_a_marker_the_prompt_leads_with_the_summary_and_folds_only_what_follows`,
    `a_legacy_fit_keeps_the_summary_when_it_drops_turns` [domain];
    `a_commit_appends_a_version_five_marker_and_the_next_request_leads_with_the_summary` [bundle];
    each adapter's `compaction_wire_tests`.
  - A session with no marker sends the request shape it always did and stays at its version. The
    domain pins the bytes (the prompt fold equals the full fold; the encoding stays version 2), the
    bundle compares the messages, and each adapter compares its wire body. Tests:
    `without_a_marker_the_prompt_is_exactly_the_full_fold`,
    `a_session_that_never_compacts_stays_at_version_two_with_no_marker_bytes` [domain];
    `a_session_without_a_marker_sends_exactly_the_request_it_always_did` [bundle];
    `a_session_with_no_marker_sends_no_summary` and its chat and Responses forms [adapters].
  - A second compaction supersedes the first, and its draft prompt contained the first summary.
    Tests: `a_second_marker_supersedes_the_first_and_the_newest_summary_leads` [domain];
    `a_second_compaction_draft_is_given_the_first_summary` [bundle].
  - The cut rule is one rule: a trailing switch back to legacy, a goal change between turns and a
    call a closed turn left unanswered do not stop a cut, and what the availability check refuses
    the write refuses too. Tests:
    `a_cut_after_a_switch_back_to_legacy_is_admitted_by_the_check_and_the_write_alike`,
    `a_goal_change_between_turns_does_not_stop_a_cut`,
    `a_call_a_closed_turn_left_unanswered_does_not_stop_a_cut`,
    `what_the_check_refuses_the_write_refuses_too` [domain].
  - Malformed markers are refused on write and on read: a `boundary` that is not its sequence, a
    wrong `supersedes`, a prefix-digest mismatch, a cut mid-turn, a result after the cut for a call
    before it, a summary that breaks the text rule or does not match its digest, and a marker in a
    body below version 5. Tests: `a_marker_whose_boundary_is_not_its_sequence_is_refused`,
    `a_marker_whose_supersedes_does_not_name_the_latest_marker_is_refused`,
    `a_supersedes_that_names_no_marker_is_refused_on_read`,
    `a_marker_with_a_wrong_prefix_digest_is_refused_on_write_and_on_read`,
    `a_marker_cut_inside_a_turn_is_refused`, `a_marker_inside_a_turn_is_refused_on_read`,
    `a_result_after_the_cut_for_a_call_before_it_is_refused_on_write_and_on_read`,
    `a_summary_that_breaks_the_text_rule_is_not_compacted`,
    `a_summary_edited_on_disk_is_refused_on_read`,
    `a_marker_in_a_body_below_version_five_is_refused_on_read` [domain].
  - The draft → preview → commit flow writes nothing on draft or cancel, refuses a stale draft, and
    leaves the file intact on a failed save. Tests:
    `drafting_a_compaction_writes_nothing_to_the_session`,
    `a_commit_after_the_session_moved_is_refused_as_stale` [bundle];
    `a_compaction_draft_writes_nothing_and_an_unknown_commit_changes_nothing`,
    `a_compaction_commit_after_a_turn_is_refused_as_stale`,
    `a_compaction_commit_the_store_cannot_save_leaves_the_file_intact` [link], which also asserts
    the `checkpoint_not_committed` code.
  - A session managed once and switched back to legacy is compacted through its checkpoint and
    takes the next turn. Tests:
    `a_session_managed_once_is_compacted_through_its_checkpoint_and_takes_the_next_turn` [link].
  - Compaction on a managed session is refused with `compact_unavailable`, and enabling managed
    context on a compacted session is refused with `compacted_session` from `nanus run`, the link,
    the interface, the runner and the domain. Tests: `compaction_is_refused_on_a_managed_session`,
    `enabling_managed_context_on_a_compacted_session_is_refused_before_any_write` [bundle];
    `a_managed_session_is_not_compacted_over_the_link`,
    `managed_context_on_a_compacted_session_is_refused_with_its_code_over_the_link` [link];
    `managed_context_on_a_compacted_session_is_refused_with_compacted_session`,
    `a_session_never_compacted_is_not_refused_as_compacted` [cli];
    `a_compacted_session_refuses_managed_context_and_the_interface_changes_nothing` [tui];
    `managed_context_after_a_marker_is_refused_on_write_and_on_read_as_compacted_session`,
    `a_compacted_session_cannot_enable_managed_context` [domain].
- **Body version.** A compacted body and a seeded body are version 5; a build reading 1 to 4
  refuses them at the header; `context/compact` in a version-2, 3 or 4 body, `context/seed` in a
  version-3 or 4 body, and an attempt with a `kind` in a version-4 body are refused.
  Tests: `a_compacted_session_is_written_at_version_five_and_reads_back_identically`,
  `a_marker_in_a_body_below_version_five_is_refused_on_read`,
  `a_seeded_session_is_written_at_version_five_and_refused_below_it`,
  `an_attempt_with_a_kind_needs_a_version_five_body` [domain];
  `a_listing_shows_every_version_a_load_reads_and_no_other` [store]. A 1-to-4 reader refusing a
  version-5 header is the existing `reads_session_version` check, tested with an unknown version.
- **Legacy isolation.** A legacy body carrying `context/seed` or `context/handoff` is refused on
  write and on read; a legacy session that never compacts keeps its body version, request bytes,
  schemas and terminal save. Tests: `a_seed_or_a_back_link_in_a_legacy_body_is_refused_on_write`,
  `a_seed_or_a_back_link_in_a_legacy_body_is_refused_on_read` [domain]; the no-marker tests above
  and the existing legacy request and save tests.
- **Maintenance requests.**
  - Both end with the fixed user instruction and carry the session's tool schemas unchanged.
    Tests: `a_compaction_request_ends_with_the_fixed_instruction_and_offers_the_schemas_unchanged`,
    `a_handoff_request_ends_with_the_fixed_instruction_and_offers_the_turns_schemas` [bundle].
  - A response with a tool call is refused and nothing executes.
    Tests: `a_compaction_response_that_asks_for_a_tool_is_refused_and_runs_nothing`,
    `a_refused_handoff_request_is_recorded_as_refused_and_nothing_is_clamped` [bundle].
  - The managed handoff request records intent before HTTP and a finished attempt with
    `kind: handoff`; a refused one is `refused`, never clamped; one that does not fit is refused
    as `candidate_too_large` before any attempt is recorded.
    Tests: `a_handoff_writes_only_its_intent_and_its_finished_attempt_both_of_kind_handoff`,
    `a_handoff_that_stopped_at_the_output_ceiling_is_refused_and_recorded_as_refused`,
    `a_handoff_that_does_not_fit_is_refused_before_any_attempt_is_recorded` [bundle].
  - A stopped draft is refused and writes nothing a reader would take as complete; over the link,
    an interrupt from the asking connection stops a draft and frees the session.
    Tests: `an_interrupted_compaction_draft_is_refused_and_writes_nothing`,
    `an_interrupted_handoff_draft_is_recorded_cancelled_and_refused` [bundle];
    `an_interrupt_stops_a_compaction_draft_and_frees_the_session` [link];
    `escape_while_a_draft_runs_stops_it_and_quits_only_once_it_has_answered` [tui].
  - A handoff intent left without an outcome is settled on its own before the next draft or turn.
    Tests: `a_handoff_intent_left_without_an_outcome_is_settled_before_the_next_draft` [bundle];
    `a_handoff_intent_left_open_in_an_idle_session_is_settled_on_its_own`,
    `an_open_turn_recovered_beside_a_dangling_intent_keeps_the_two_apart` [domain unit].
  - The legacy compact request's usage reaches the marker, `usage_totals` and `usage_by_model`, and
    a handoff draft's usage reaches the totals without double-counting a turn step.
    Tests: `a_compaction_drafts_usage_reaches_the_marker_and_the_sessions_totals` [bundle];
    `usage_totals_include_the_usage_of_a_compaction`,
    `usage_by_model_attributes_a_compaction_to_the_model_that_wrote_it`,
    `a_handoff_drafts_cost_is_in_the_sessions_totals_and_a_turn_steps_is_not_counted_twice`
    [domain].
- **Link.** A v12 client against a v11 agent is refused at the handshake; a `draft_id` that is not
  the held one is `stale_draft`; two viewers' drafts resolve with one commit and one `stale_draft`;
  a refusal's sentence never repeats its code; a maintenance request with no session attached is
  refused with its code. Tests: `a_v12_client_is_refused_by_an_agent_at_11` [link client];
  `a_wrong_handoff_id_keeps_the_held_draft`,
  `the_first_compaction_commit_wins_and_the_other_draft_is_stale`,
  `the_first_handoff_commit_wins_and_the_other_draft_is_stale`,
  `a_refusal_says_its_code_beside_a_bare_sentence`,
  `a_maintenance_request_with_no_session_attached_is_refused_with_its_code` [link].

- **Review fixes (2026-10-21).** Each is tested in both directions.
  - A frontier survives a body-version move, and a changed prefix is still stale. Tests:
    `a_frontier_taken_before_the_body_moved_version_still_describes_its_prefix`,
    `a_frontier_whose_digest_matches_no_version_of_the_header_is_refused` [domain unit];
    `a_session_that_hid_history_still_runs_turns_and_drafts_after_a_handoff_draft` [bundle].
  - A seeded session switched to legacy leads with its seed; a managed one leaves it to the
    compiler; after a cut the summary stands in for it. Tests:
    `a_seeded_session_switched_to_legacy_still_leads_its_prompt_with_the_seed`,
    `a_seeded_session_still_managed_leaves_the_seed_to_the_managed_compiler`,
    `after_a_cut_the_summary_stands_in_for_the_seed` [domain];
    `a_seeded_session_switched_to_legacy_still_leads_with_its_seed` [bundle].
  - The text rule refuses invisible formatting characters and whitespace-only text, and looks
    through joiners. Tests: `an_invisible_format_character_is_refused_wherever_it_sits`,
    `a_joiner_cannot_hide_a_label_but_is_allowed_in_ordinary_text`,
    `text_that_is_only_whitespace_is_refused_as_empty` [domain unit].
  - A call id reused after a cut is answered like any other. Test:
    `a_call_after_the_cut_that_reuses_a_severed_id_is_answered_like_any_other` [domain].
  - Seed and back-link placement are checked on write and read. Tests:
    `a_seed_misplaced_in_the_log_is_refused_on_write_and_on_read`,
    `a_seed_after_a_legacy_mode_record_is_refused_on_write_on_read_and_by_the_fold`,
    `a_back_link_must_name_the_session_that_holds_it`,
    `a_repeated_back_link_is_refused_on_write_not_only_by_the_fold` [domain].
  - A handoff draft recovers an open turn first; an oversized document is recorded `refused`; a
    fitted compaction says what it dropped; the instruction names the header fields. Tests:
    `a_handoff_closes_a_turn_a_crash_left_open_and_then_drafts`,
    `a_legacy_session_with_an_open_turn_is_refused_before_anything_is_recovered`,
    `a_handoff_whose_document_would_pass_its_bound_is_recorded_refused_not_completed`,
    `a_compaction_whose_fit_drops_an_old_turn_says_so_and_still_asks_the_model`,
    `the_handoff_instruction_names_every_header_field_and_the_compaction_one_names_none`,
    `a_coded_refusal_gives_its_code_and_a_bare_sentence_and_nothing_else_has_a_code` [bundle].
  - Link: a quarantined source is not handed off; an interrupt straight after a draft request
    stops it; a draft that ends after its connection moved on sends nothing; only the asking
    connection stops a draft. Tests:
    `a_handoff_commit_from_a_quarantined_source_is_refused_before_anything_is_written`
    [link unit]; `an_interrupt_sent_straight_after_a_draft_request_stops_it`,
    `a_draft_that_ends_after_its_connection_moved_on_sends_it_nothing`,
    `only_the_connection_that_asked_for_a_draft_can_interrupt_it` [link].
  - Interface: session state follows the attachment; prompts wait for a draft or a switch; a
    handoff drops queued prompts; a failed switch ends and says the old conversation was let go;
    controls wait for each other; the clear warning follows a turn that starts under it; a
    recording's refusals carry codes. Tests:
    `a_clear_attachment_forgets_the_old_sessions_title_tokens_and_mode`,
    `a_handoff_attachment_is_managed_and_titled_by_the_new_sessions_name`,
    `a_prompt_typed_while_a_draft_runs_waits_until_the_draft_is_over`,
    `a_prompt_typed_while_a_switch_is_pending_is_sent_after_the_attachment`,
    `a_committed_handoff_discards_the_prompts_queued_for_the_old_conversation`,
    `a_failed_new_conversation_ends_the_interface_with_the_reason_and_the_way_back`,
    `a_handoff_that_could_not_be_attached_ends_the_interface`,
    `a_failure_with_no_switch_pending_does_not_end_the_interface`,
    `a_turns_failure_during_a_switch_leaves_the_switch_pending`,
    `a_session_control_waits_for_a_pending_draft_or_switch`,
    `a_stray_attachment_changes_neither_the_view_nor_the_source`,
    `a_turn_that_starts_under_the_clear_warning_is_named_and_asks_for_quiet_again`,
    `a_recordings_handoff_and_compaction_are_refused_with_their_codes` [tui].

Not verified, and not claimed (consistent with the existing contract): **no live provider has been
sent a maintenance request** (fixtures only); no quality or cost evaluation of compaction or
handoff has been run; power-loss durability is not claimed; native Windows is cross-target lint
only.

As built, also tested, in both directions: a naming failure after the seed is saved, over the real
store and link, reports `Seeded.name` as absent; the exit summary follows the attachment after a
clear and after a handoff, and a connection that never switched summarises its own session; the
handoff records' schema is checked with the managed records; a compaction's divider, the seed
block, the stale refusal's code, the preview's dropped-turns wording and its cut count are drawn by
the interface's runtime tests; and the kept layout draws the seed and the cut as a fresh one does,
through the tool-collapse toggle.

As built, not verified, and not claimed: an unknown checkpoint outcome during a handoff commit is
settled by `settle_uncertain`, which is tested on its own the three ways
(`an_uncertain_handoff_write_is_settled_against_the_disk_and_never_written_again` [link unit]),
but no link test injects an unknown outcome into a whole handoff commit, because the link's
integration tests run over the real store; the interface's drawing is checked at the layout and
runtime layers, not in a terminal; no live provider has been sent a handoff or compaction draft,
and no summary has been judged for quality.

## Deviations and open questions

- **`/clear` naming.** This spec promotes the shipped `/clear` to the session-level control and
  keeps the pure screen erasure on `Ctrl+L`. The alternative — keep `/clear` screen-only and add
  `/new` — is a one-word change to the command table and tests. The owner should confirm which
  spelling the shipped interface uses.
- **Handoff destination.** The handoff lives in the new session as a seed record. Writing
  `HANDOFF.md` into the workspace is a filesystem side effect and is excluded; it can be added
  later.
- **Compaction reversibility.** The cut is forward-only for the model. The raw log and the
  transcript keep the original for the human, but a legacy model cannot read it back. A future
  "un-compact" would be a record that retires the active marker; this spec leaves it out.
- **Compaction in managed sessions.** Deliberately out of scope. Managed context's own hiding and
  hard fitting cover it, and a cut there would need an exception to the protected floor.
- **Command-line use.** `nanus run --handoff` and `--compact` are deferred. A one-shot command
  cannot show a draft and ask for a `y`, and defining a non-interactive confirmation (a named flag
  a human types, like `all_calls`) and what it prints on stdout is its own decision.
- **Cost guardrails.** A draft spends a model call before any human sees it, which is the price of
  previewing before committing. A cancelled legacy compact draft's spend is reported but not
  recorded in the session; a handoff draft's is recorded, in its attempt, and counted. Whether
  drafts should be refused past a per-session spend ceiling — no such ceiling exists today — is
  open.

**As built (deviations from this document, and why).**

- **Refusal code.** `Frame::Refused` carries the code in `code`, not in the message, so a refusal's
  sentence and its stable code are separate fields, and the sentence is the bare one
  (`BundleError::reason`). An interface that reads only the sentence still works.
- **Naming after the seed.** A name that fails after the seed is saved does not undo the handoff;
  the seed's `name` reports what the session holds. The spec's "carry on" is kept, with the report
  made explicit. A name is checked against the store's rules, and for being one word, before the
  model is asked, so this is now only a store failure, not a rule the draft could have caught.
- **Attachments and the exit summary.** The interface follows the attachment the agent makes after
  a clear or a committed handoff, so what it reports on exit is the session the connection is in.
- **Collapse.** The seed's document and the compaction summary both fold under the tool-collapse
  toggle (`Ctrl+T`), as the Link protocol section says; they are drawn unfolded until a reader folds
  them.
- **Drafts.** A handoff draft invalidates a second handoff draft of the same session, by its
  attempt records. Drafts run in their own task and stop on `Interrupt`.
- **Fitting and refusals.** Prepare-time refusals write no attempt record; a handoff is refused
  rather than fitted; a draft whose fit left no real turn is `compact_unavailable`; truncated
  replies are refused.
- **Usage.** `usage_by_model` includes a compaction's usage under the model that wrote its summary,
  and a handoff draft's finished attempt under the attempt's model, so the per-model figures add up
  to `usage_totals`.
- **`CheckpointReason::Handoff`** was added for the seeded session's first save and the back-link,
  so a checkpoint's reason says what it is for.
- **Availability.** `Session::can_compact` and `Session::turn_open` are the domain's answer to
  availability, and `can_compact` asks the same cut rule the write does.
- **Seed placement.** The seed is a leading system message rather than part of the generated
  message, because the generated message cannot exist on a seeded session's first request.
- **Version 5.** Version 5 covers every maintenance record, not only the compaction marker, because
  a version-4 reader would fail part-way through a body holding a seed, a back-link or an attempt
  with a `kind` just as it would on a marker.
- **Queued prompts.** A confirmed clear discards the old conversation's queued prompts, with a
  notice, rather than sending them into the new session or keeping them for later.
- **Confirmation keys.** The previews and the clear warning take the approval dialog's settle
  delay, so a `y` being typed when one appears never commits it.
- **Seed after a switch to legacy.** This document first said nothing about a seeded session that is
  switched to legacy context, and the code then dropped the seed from its prompt without a word.
  The seed is now kept as the legacy prompt's leading system message, until a cut supersedes it.
- **Interrupt ownership.** A draft is stopped only by the connection that asked for it, which is
  what "from that connection" in the Flow meant; a turn is still stopped by any viewer.
- **A failed switch ends the interface.** The agent releases the old conversation before it opens
  the next one, so a clear whose `New` fails, or a handoff whose new session cannot be attached,
  leaves the connection following none. For a clear, `New` carries no name, so it fails only when
  the store cannot claim the new session (an I/O fault) or the connection has fallen too far
  behind. The interface then exits non-zero, printing the agent's reason on stderr with the old
  session's id and `nanus tui --resume <id>` (or, for a handoff, `nanus sessions`). The agent is
  not stopped: the failure is this connection's, and stopping the agent would end every other
  session's running turn unrecorded. A turn's own failure while a switch waits does not end it.
