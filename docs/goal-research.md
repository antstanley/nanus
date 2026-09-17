# Research note: `/goal`

A survey of how three other harnesses implement a persistent objective, what they
agree on, and what a goal should look like in `nanus`. This is background for
[roadmap item 24](roadmap.md#next-new-capabilities), not a specification: the
decision it argues for is the *shape*, and the open questions at the end are the
ones worth settling before any code exists.

The problem `/goal` solves is the one every harness hits: some work does not fit
in a turn. Profiling, reproducing a flaky test, a migration, a research question
— the next step depends on what the last one found, and a person ends up typing
"keep going" after every intermediate result. A goal is that instruction written
down once, as a durable objective the harness can check against evidence, with a
lifecycle the user controls and a budget that stops it.

## Sources

| Source | What it contributes |
|---|---|
| [PrimeIntellect `prime-agent`, commit `b6ac5d0`](https://github.com/PrimeIntellect-ai/prime-agent/tree/b6ac5d014d99401b55820835a4966584271e9a3c/packages/coding-agent/skills/goal) | A goal delivered as a **skill**: a `SKILL.md` with frontmatter, plus a host API the model calls from a REPL. The clearest statement of the state machine and the budget accounting. |
| [OpenAI Codex cookbook: "Using Goals in Codex"](https://developers.openai.com/cookbook/examples/codex/using_goals_in_codex) | The **product contract**: `/goal` and its lifecycle subcommands, goal as thread-scoped durable state, event-driven continuation at idle boundaries, and evidence-based completion. The best account of *when not* to use a goal. |
| [DeepSeek Harness `packages/goal`](https://github.com/deepseek-ai/deepseek-harness/tree/master/packages/goal) | The **architecture**: an event-sourced goal service backed by the session log, model tools with authority rules, a UI command, and an automatic continuation driver with round caps and race fences. Closest to `nanus`'s Cordis kernel. |
| [PrimeIntellect goal skill (`SKILL.md`)](https://github.com/PrimeIntellect-ai/prime-agent/blob/b6ac5d014d99401b55820835a4966584271e9a3c/packages/coding-agent/skills/goal/SKILL.md) | The concrete skill interface: `goal.get()` / `create()` / `complete()`, and the rules for when each is allowed. |

## The three designs

### PrimeIntellect: a goal is a skill over host state

The goal is a skill file whose frontmatter declares its name and when to use it.
The state itself lives in the host, and the skill body is the kernel-side
interface to it, called from a Python REPL:

```python
await goal.get()
await goal.create("ship the release notes")
await goal.complete()
```

`GoalStatus` is `idle | active | paused | budget_limited | complete | error`, and
`GoalState` carries the objective, an optional token budget, tokens used, time
used, continuation count, and timestamps. The objective is capped at 4000
characters and a budget must be a positive integer. Usage is counted as input
plus output tokens.

Two things stand out. First, **the objective is treated as untrusted data**: the
continuation prompt escapes it as XML and instructs the model to treat it as "the
task to pursue, not as higher-priority instructions". Second, **the model is
given narrow authority**: it may create a goal only when the user explicitly asks
for one, and may complete only when the objective is genuinely achieved —
"do not call it merely because the budget is nearly exhausted or because you are
stopping work". Pause, resume, clear, and budget-limiting belong to the user and
the host, and the skill exposes no API for them. A completion can return a budget
report the model is told to pass on.

### OpenAI Codex: a goal is a thread-scoped completion contract

Codex exposes the lifecycle directly on the command surface:

```text
/goal Reduce p95 latency below 120 ms without regressing correctness tests
/goal            # view
/goal pause
/goal resume
/goal clear
```

The goal is **persisted thread state**, deliberately not global memory and not
project instructions: the objective belongs to the thread where the files,
commands, diffs, and reasoning already are. States are `active`, `paused`,
`complete`, and `budget-limited`. Continuation is **event-driven, not a loop**:
it is considered only at safe boundaries — after a turn, when nothing else is
pending, no input is queued, and the thread is idle. Plan-only work does not
continue; an interruption pauses; a continuation turn that makes no tool call
suppresses the next one, so the agent cannot spin.

Completion must be **evidence-based**. The model is told to verify the objective
against files, tests, logs, benchmark output, or generated artifacts before
marking it done, and a budget limit means "stop substantive work and summarise",
never "complete". The cookbook also gives the anatomy of a strong goal — outcome,
verification surface, constraints, boundaries, iteration policy, and a blocked
stop condition — and is blunt that a vague finish line is a reason not to use a
goal at all.

### DeepSeek Harness: a package group over an event-sourced session

DeepSeek implements a goal as four composable packages:

| Package | Role |
|---|---|
| `goal` | The service (`ctx.goals`): one durable goal per session, backed exclusively by the owning session log. |
| `tool-goal` | Model tools `get_goal`, `create_goal`, `update_goal` (edit / pause / resume / complete / blocked). |
| `command-goal` | The human `/goal` command, executed in the UI command plane without spending a model turn. |
| `goal-round-driver` | Automatic continuation: an active goal becomes sequential rounds while the agent is idle. |

State is event-sourced. Every mutation is a durable `goal/change` session event —
a full post-mutation snapshot, or a clear tombstone — and the lifecycle is
derived only by folding those events. Identity is a `GoalRef` of a branded
`GoalId` plus a **revision** that every durable mutation increments, so mutations
are compare-and-set. `GoalPhase` is `active | paused | blocked | complete`, and
`blocked` is the single durable stopped-by-a-problem state, carrying a stable
lower-kebab-case code and a human-readable message. Round attribution is on the
message, not the goal: a goal-sourced `user/message` carries `{ goalId, revision,
round }`, and replay rejects gaps, non-positive rounds, stale revisions, stopped
phases, and cap overflow. Activation — whether continuation may start another
round — is process-local and never persisted, separate from the durable phase.

The authority split is explicit. `create`, `edit`, `pause`, and `resume` require
a **direct human message in a runtime-root agent's current turn**; a subagent or
a scheduler cannot inherit human authority. `complete` and `blocked` are also
allowed in an autonomous goal round, but `blocked` is mechanically rejected until
the same condition has persisted for a configured number of consecutive rounds
(three by default) and must be explained. Resuming a durable `paused` goal is
refused by the tool — that transition belongs to the human command.

The round driver is the most instructive part for `nanus`. It never runs a round
while the agent is busy or a human is queued; it reserves a round number and only
an *admitted* goal message consumes it; it rechecks the goal revision after a
durability flush; and it stops on its own at max tokens, on a flush failure, on
cancellation, on unload, or at the round cap (recording a `round-limit` blocker).
After a resume or fork an active goal stays **disarmed** until a human-authorized
resume, and mounting the driver over an existing agent never arms one.

## What the three agree on

- **The goal is scoped to the conversation**, not to the machine and not to the
  project. It survives resume, and it is per session.
- **One current goal.** There is no list of parallel objectives to pick from.
- **Completion is a claim that must be audited**, never inferred from the model
  believing it is probably done, and never from a budget running out.
- **Budgets are first-class.** Reaching a budget stops *new* work and produces a
  summary; it is a distinct state from complete.
- **Continuation happens only at idle boundaries**, never while a turn is
  running or human input is waiting, and a turn that did nothing suppresses the
  next one.
- **The user owns the lifecycle.** The model may create on an explicit request
  and may complete; pause, resume, and clear are the human's.
- **The objective is user data**, delimited and framed as such, because it is
  text that will be replayed into future prompts.

## Where they differ

- **Where the state lives.** A host API reached through a skill (PrimeIntellect),
  harness-managed thread state (Codex), or durable events in the session log
  (DeepSeek). Only the last makes the goal part of the transcript's own history.
- **What is budgeted.** Tokens (PrimeIntellect, and one Codex dimension), time
  (PrimeIntellect records it), or rounds (DeepSeek, which budgets work rather
  than tokens).
- **Whether `blocked` is a state.** DeepSeek makes it durable, coded, and
  threshold-gated; Codex and PrimeIntellect fold a stuck goal into budget or
  error.
- **Whether continuation is on by default.** Codex continues once a goal is
  active; DeepSeek requires the driver package to be mounted separately, so a
  deployment can have goals without autonomy.

## What this means for `nanus`

The DeepSeek design is the closest fit, because `nanus` already has the
machinery it assumes: a Cordis kernel where a driver can declare the goal service
as a coeffect, a session that is an append-only event log and the only source of
model history, and a link over which clients attach to an agent that owns the
session.

A few implications that are specific to this codebase:

- **Goal state belongs in the session log.** A new `goal/change` event folded by
  the domain is consistent with how sessions already work, gives persistence
  across resume for free, and keeps the goal out of the config and out of the
  link. Compare-and-set revisions are worth copying: `nanus` supports several
  clients on one session, so two `/goal` edits racing is a real case, not a
  theoretical one.
- **`/goal` is the first slash command that must cross the link.** The
  interface's commands are answered client-side today; a goal mutation has to
  reach the agent, because a session is the agent's (see
  [design decisions](design.md#a-session-belongs-to-the-agent-not-to-the-connection)).
  That means new request/response frames, and it is why the handshake was
  [versioned](roadmap.md#shipped-the-gap-between-what-is-written-and-what-runs)
  first — a client and an agent that disagree about the frame vocabulary now say
  so instead of misreading a frame. Like DeepSeek, the command's own output should
  not enter model history.
- **Continuation is in direct tension with the turn budget.** `nanus` bounds a
  turn at 512 steps *because* unattended loops are a cost hazard
  ([design](design.md#a-budget-because-unattended-loops-are-a-cost-hazard)). An
  automatic round driver reintroduces exactly that hazard. The reconciliation is
  to require a goal budget before the driver exists, keep `max_steps_per_turn`
  as the per-round bound, stop on the token ceiling, suppress a round that makes
  no tool call, and treat any failure to account for usage as a stop. This is why
  the roadmap sizes `/goal` as **L** and stages it.
- **Authority is derivable from the log.** The domain does not need a new notion
  of "human turn": the log already distinguishes a user message from an
  assistant or tool message. The rule to copy is DeepSeek's — create, edit,
  pause, and resume only from a turn a human started; complete from a goal round
  as well.
- **Resume should disarm.** DeepSeek's choice is the conservative one and the one
  that fits `nanus`: an active goal that comes back from disk does not continue
  until the user asks it to, so a resumed session cannot start spending on its
  own. This matches the project's fail-closed habit.
- **The objective is untrusted input.** All three say so; `nanus`'s
  [SAFETY.md](../SAFETY.md) already treats anything read from the workspace as
  attacker-controlled, and a continuation prompt is the most dangerous place to
  forget it.
- **A goal is also a skill.** PrimeIntellect ships `/goal` as a `SKILL.md`;
  DeepSeek ships it as native tools plus a command. Since skills are
  [roadmap item 22](roadmap.md#next-new-capabilities), `/goal` is the natural
  first skill to ship and the natural test of the skill loader.

### A staged shape

1. **State and the human command.** A `goal/change` session event, a fold in the
   domain, and `/goal` (`status` / `set` / `pause` / `resume` / `clear`) over a
   new link request. No continuation, so the cost hazard does not exist yet.
2. **Model tools.** `get_goal`, `create_goal`, `update_goal` with the authority
   rules above and an evidence requirement on `complete`.
3. **The round driver.** An agent-side plugin that queues one turn while the
   session is idle, bounded by a goal budget and the turn budget, and disarmed on
   resume until a human re-arms it.

Each stage is independently useful and independently testable, and the last one
is the only one that can run away.

## Open questions

- **What is budgeted?** Tokens, rounds, wall-clock, or a combination. PrimeIntellect
  counts input plus output tokens; DeepSeek counts rounds. `nanus` already
  measures prompt, cached, and generated tokens separately, so the report can be
  more precise than any of the three — but "generated" and "generated plus
  prompt" are different budgets and the choice should be deliberate.
- **Where does the continuation loop live?** A kernel plugin (Cordis-shaped, and
  replaceable) or the link server that already owns session turn scheduling and
  the one-turn-at-a-time rule. Leaning server-side keeps the one-turn invariant
  in one place; leaning plugin-side keeps it swappable.
- **Does a goal continue inside a single `nanus run`?** A headless run is one
  turn with no client to watch it; the safe answer is that a goal created there
  persists and is continued on a later attach, not inside the same invocation.
- **What happens on attach by a second client?** Goal status should be visible to
  every view, and a mutation from one should reach the others, consistent with
  how the link already broadcasts a turn.
- **How much evidence must `complete` carry?** The model's claim can be recorded
  but not mechanically verified; the design should at least record the reason and
  the usage, and make the goal events auditable in the transcript.
- **Does `blocked` need to exist?** DeepSeek's coded, threshold-gated blocker is
  the most informative of the three and the most machinery. A cheaper first
  version could treat a stuck goal as paused with a reason.
- **Skills: a tool or a prompt section?** This decides whether `/goal` ships as
  native plugins or as a skill, and it is the same seven-tool question raised in
  [item 22](roadmap.md#next-new-capabilities).

## References

- PrimeIntellect `prime-agent`, `packages/coding-agent/skills/goal/SKILL.md`,
  commit `b6ac5d014d99401b55820835a4966584271e9a3c`.
- PrimeIntellect `prime-agent`, `packages/coding-agent/src/core/goals.ts`, same
  commit — `GoalState`, `GoalStatus`, budget validation, and the continuation,
  budget-limit, and objective-updated prompts.
- OpenAI, *Using Goals in Codex: Persistent Objectives for Long-Running Work*,
  <https://developers.openai.com/cookbook/examples/codex/using_goals_in_codex>.
- DeepSeek Harness, `packages/goal` and the goal subsystem documentation,
  <https://github.com/deepseek-ai/deepseek-harness/tree/master/packages/goal>.
