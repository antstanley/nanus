# Roadmap

What is planned, in the order it is worth doing, with a rough effort attached.
This is a plan rather than a promise: the sizes are relative guesses by someone
who knows the codebase, not dates, and nothing here has an owner. Where an
"obvious" feature is deliberately absent, it is in
[by design, not planned](#by-design-not-planned) with the reason, because the
[design decisions](design.md) that explain why are usually the interesting part.

Every item in [what is not supported yet](features.md#not-supported-yet) appears
here. A few more come from the limits recorded in
[status](status.md#known-limits), [sessions](sessions.md#known-limits),
[service](service.md#known-limits), and [the interface](tui.md#what-is-deliberately-missing).

## How to read the sizes

| Size | Rough shape |
|---|---|
| **XS** | A focused change in one place — an afternoon at most. |
| **S** | About a day: one crate, its tests, and a doc line or two. |
| **M** | Several days: a new module or a small protocol change, plus integration tests. |
| **L** | A week or more: a new crate or a cross-cutting change that wants a design note first. |
| **XL** | Architectural: it changes a load-bearing invariant or adds a platform. |

Sizes assume one person familiar with the repository. They are not additive, and
an item that depends on another is noted — doing the dependency first usually
makes the dependent cheaper.

## Shipped: the gap between what is written and what runs

These were first because they were correctness rather than new surface, and they have
landed. The numbers are kept because later items refer to them ("depends on 2"), and each
entry says where the work lives rather than what it was going to be.

| # | Item | Where it landed |
|---|---|---|
| 1 | **Approval is enforced at the tool boundary.** A tool declares what it can touch, the sandbox decides what runs unasked, and a call outside it is denied unless an answerer grants it — `per_call` failing closed when nobody can answer, `permitted` granting the non-destructive exceptions, and `all_calls` granting every exception. The system prompt carries the runtime policy. | `nanus-domain/src/approval.rs`, `nanus-bundle/src/agent_loop.rs` |
| 2 | **The interface and the CLI answer it.** An `approval` frame and an `approve` request cross the link, the interface draws a dialog and answers, and `nanus run` prompts on the terminal — or denies, when stdin is not one. | `nanus-link/src/{protocol,server}.rs`, `nanus-tui/src/{runtime,view}.rs`, `nanus-cli/src/approve.rs` |
| 3 | **A step's tool calls run together, bounded by `max_parallel_tools`.** Cooperative concurrency with results recorded in call order, and decisions kept sequential so an approval question is asked one at a time. | `nanus-bundle/src/agent_loop.rs` |
| 4 | **The live tool-call frames are verified.** A real `api.deepseek.com` tool-call response is replayed byte for byte; it decoded with no change to the wire. | `nanus-bundle/tests/data/`, `live_wire.rs` |
| 5 | **The link handshake is versioned.** A mismatch is a sentence naming both versions rather than a decode error mid-turn, and an unversioned handshake is refused rather than assumed compatible. | `nanus-link/src/{protocol,client,error}.rs` |
| 6 | **`nanus sessions delete <ref>` removes a session**, resolving the reference as naming does and refusing one that answers to nothing. The interface has no key for it yet. | `nanus-cli/src/cli.rs` |

## Shipped: what a second review found

The items above were the gap between what is *written* and what *runs*. A second review read
every crate in dependency order and looked for the opposite — behaviour that is enforced and
wrong where two parts meet, which is where a test built from one component at a time cannot
look. What it found has landed, and it is the same kind of work: correctness rather than new
surface. None of it takes an item number, because the numbers are the plan's and later items
refer to them ("depends on 2"); an unnumbered list here records what shipped without moving
a reference.

| What landed | Where it lives |
|---|---|
| **A withdrawal waits for the deactivations it causes, at any depth.** Unloading a provider retires its bindings, sweeps the whole cascade while they still resolve, and only then takes them away — so a dependent can hand back what it borrowed even when the thing being unloaded is two hops away. The kernel could do this one level deep, and a chain of three lost the middle binding. | `nanus-kernel/src/context.rs`, `tests/composition.rs` |
| **A tool result is paired with the call it answers, by identity.** A step writes every call it made and then every result, so position could not pair them: a two-call step drew its first call as still running, under the second tool's name, and its last result twice. The replay pairs by the log's `call_id`, and the link now carries that id on both tool frames so the live view pairs by identity too — falling back to name and order only for a frame from an agent that predates the field, which is why `PROTOCOL_VERSION` did not move. | `nanus-tui/src/{replay,view,transcript}.rs`, `nanus-link/src/protocol.rs`, `nanus-bundle/src/agent_loop.rs` |
| **`bash` runs in the workspace root by default**, which is what its schema and the system prompt both promised and what the process's own directory was not: the two agreed only while `workspace_root` was unset, and a service inherited its directory from the shell that started it. | `nanus-bundle/src/tools/bash.rs` |
| **The agent advertises the toolset it dispatches from.** The runner and the published `tools` service were two registries built from the same ports, so registering a tool changed the count in the handshake and nothing about the requests. There is one registry now, with a `ptr_eq` postcondition where the composition is mounted. | `nanus-bundle/src/compose.rs`, `agent_loop.rs` |
| **A malformed optional argument is a correction rather than a default.** `Arguments` exists so a bad call becomes a message the model can act on, and every tool was reading optional fields with `unwrap_or(None)` — so `{"limit": "ten"}` quietly meant the default. The same shape of defect as item 1: a rule that was written down and then never applied on the path that mattered. | `nanus-bundle/src/tools/*.rs`, `args.rs` |
| **`serial` and `bail` are two modes rather than one function**, which is what their documentation had claimed all along. | `nanus-kernel/src/event.rs` |
| **Smaller, and the same kind of thing:** `Ctrl-C` at an approval prompt did nothing, because the turn is asleep on the answer and the stop flag never reached a checkpoint; `--scroll` without `--session` was accepted and then ignored; a capped search reported `truncated` whenever the cap was *reached* rather than when a match was dropped; `tools_plugin` swallowed a toolset that failed to build into an empty registry; and several doc comments described behaviour that had changed under them, including the one that called `danger_full_access` "no confinement" when the filesystem tools are rooted whatever the mode. | `nanus-cli/src/{approve,cli}.rs`, `nanus-adapter-local/src/fs.rs`, `nanus-bundle/src/lib.rs`, `nanus-domain/src/approval.rs` |

The findings in full, why the suite could not see them, and the tests that pin them are in
[the bugs a certificate review found](testing.md#the-bugs-a-certificate-review-found).

## Shipped: the readings a harness comparison needs

A study of harness cost across models ([HarnessTax](https://harnesstax.github.io/)) measures a
harness by two things nanus could not report: what a run *cost*, and what configuration
produced it. Neither was a correctness bug; both were an instrumentation gap, and the data was
already being recorded and then thrown away.

| What landed | Where it lives |
|---|---|
| **`nanus sessions show [--json] <ref>` reports a run**, from the log alone: the configuration, the turns, steps and requests, prompt tokens split into cached and read, generated tokens and how many were thinking, a per-model breakdown, and why the last turn ended. It composes nothing and reads no key, and `nanus run --verbose` prints the same totals in one line on stderr as the turn finishes. The figures existed — `RunOutcome` already carried `usage_totals()` and nothing printed it. | `nanus-cli/src/cli.rs`, `nanus-domain/src/session.rs` |
| **A session records what produced it.** The header carries the configured model, the reasoning effort, the sandbox mode, the approval policy and the release; each model turn carries the model and effort that produced it, because a session can be resumed against a different model and a header-only record would describe the first request as though it described all of them. The effort is asked of the adapter, which is the component that fills in an unset one. Every field is optional and absent means *not recorded*: the session format version did not move, because a header field is not a change to how the body is read, and moving it would have made every existing transcript unreadable. | `nanus-domain/src/session.rs`, `nanus-bundle/src/{compose,agent_loop}.rs`, `nanus-ports/src/llm.rs` |

Still open from the same study, and deliberately not done here: no figure is denominated in
money, so a run's cost is reported in tokens and whatever price list the reader brings; and
there is no task suite to run a comparison *over*, which is the item that would turn these
readings into a result.

## Shipped: what the interface needed to be a daily driver

The interface items, taken in the order they were worth doing, and where each one landed. The
numbers are the plan's, kept because later items refer to them.

| # | Item | Where it landed |
|---|---|---|
| 7 | **Syntax highlighting in fenced code blocks.** A lexer inside the view rather than a highlighting crate: nothing is read from disk to draw an answer, a construct that spans lines stays itself across them, and a language it does not know is drawn verbatim. The classes borrow the role styles, so `NO_COLOR` keeps the modifiers and loses only the colours. | `nanus-tui/src/markdown/{highlight,theme,render,wrap}.rs` |
| 8 | **A help overlay for the key list (`?`).** One table in the crate, drawn as an overlay and scrollable, opened by `?` on an empty prompt and closed by the keys that leave any other dialogue. The gate is the composer: `?` is still a `?` in a prompt. | `nanus-tui/src/help.rs`, `view.rs`, `runtime.rs` |
| 9 | **More slash commands.** `/help` and `/clear` landed. Both act on the screen rather than the session, so both are answered in a recorded transcript as well as a live one, and `/help` draws the same overlay `?` does rather than a second list. The remaining two the item named are not free standing: `/model` lands with item 10, and `/compact` is a policy for the context window, which is item 21. | `nanus-tui/src/command.rs`, `runtime.rs` |
| 10 | **Switch model at runtime (`Alt+P`).** The list of ids crosses the handshake, `Alt+P` cycles it and `/model <id>` names one, a `SetModel` request carries the choice to the agent, and `ModelChanged` tells every watcher. The choice persists for the agent's lifetime rather than the turn's, the same rule the approval state has, and the id is refused by name when the agent does not offer it. The system prompt keeps naming the startup model, as it keeps naming the startup approval state. | `nanus-bundle/src/{agent_loop,compose,lib}.rs`, `nanus-link/src/{protocol,server}.rs`, `nanus-tui/src/{runtime,view,command}.rs` |
| 11 | **Extended-thinking toggle (`Alt+T`).** The reasoning scale gained a step, the link gained `EffortState` with a `SetEffort` request and an `EffortChanged` frame, and the runner carries the chosen effort into the next request instead of leaving the adapter's default in place. It is a cycle rather than a toggle, because only one of the four steps means "no thinking". | `nanus-ports/src/llm.rs`, `nanus-bundle/src/agent_loop.rs`, `nanus-link/src/{protocol,server}.rs`, `nanus-tui/src/{runtime,view}.rs` |
| 12 | **Permission-mode switching (`Shift+Tab`).** The blind cycle became a dialog: it lists the three approval states with what each one means, opens on the state the old binding moved to so the keystrokes a reader knows still work, and moves only the knob the agent can be told about. It says where the other one is set, because a reader choosing between three states deserves to know what the sandbox already permits. | `nanus-tui/src/{view,runtime}.rs`, `docs/tui.md`, `SAFETY.md` |
| 13 | **Paste an image (`Ctrl+V`).** The clipboard is read by the platform's own tool — `pbpaste`, `wl-paste`, or `xclip` — and the bytes are checked by their magic number, because a reader that answers with text has not provided an image. The image is written into the workspace (`.nanus/pasted/`, `.nanus/` is the harness's) and its *path* goes into the prompt, because `read_image` takes a path and the wire has no way to carry bytes a client produced. | `nanus-tui/src/paste.rs`, `runtime.rs`, `docs/tui.md` |
| 14 | **`@` file mentions.** A bounded walk of the workspace with three machinery names skipped, a ranking that puts a file's own name above a path that contains it, and a menu above the composer that `Tab` completes, the arrows choose and `Esc` dismisses. It expands to the *path*, because inlining contents would spend the context on a file the model never asked for. The walk is remade whenever a mention starts, so a file the agent just wrote is offered. | `nanus-tui/src/mentions.rs`, `runtime.rs`, `view.rs` |
| 15 | **`!` bash mode.** A line that opens with `!` is a shell command the interface runs itself, echoed into the transcript with its output bounded and its exit status named. It is a task rather than a blocking wait, so a slow command does not stop a running turn's frames being read, and the composer's mark becomes `$` so the mode is visible before `Enter`. Nothing about it reaches the session log or the model, and it is not confined — which is why the safety note landed first, in [SAFETY.md](../SAFETY.md) and [the interface](tui.md#running-a-command-yourself-with-). | `nanus-tui/src/{shell,command,runtime,view}.rs`, `SAFETY.md` |

## Next: sessions and the link

The numbers skip 16: an item was taken out of the plan and the number was left unfilled rather than
reused, so every reference from 17 onwards still means what it said when it was written.

| # | Item | Size | Notes |
|---|---|---|---|
| 17 | **Let a client attaching mid-turn catch up.** | **M** | A client that joins late sees the rest of the turn and no more. The transcript is already in the store, so this is a decision about seeding the view from the log rather than a new source of truth. |
| 18 | **Propagate a rename to a held session.** | **S** | The store updates immediately and resolving works; a *listing* of held sessions can show the old name until the agent next opens it. |
| 19 | **Lock a session, or refuse a second writer.** | **M** | Two agents can resume one conversation and the second save wins. A lock file or a store-level check with a clear error, keeping attaching to a live session as the supported path. |
| 20 | **Decide names: namespaces and case.** | **S** | Names are flat and case-sensitive today, so `Nightly` and `nightly` are two names. Small to change; mostly a decision. |
| 21 | **Manage the context window.** | **L** | Not in [features.md](features.md), but implied by it: prompt assembly replays the whole log, so a long session eventually exceeds the model's window. A summarise-or-drop policy, deterministic and tested, because a silent truncation is worse than a refusal. |

## Next: new capabilities

Four additions larger than a feature but short of a rewrite: a way to package
behaviour, a way to hold a secret, a goal that outlives a turn, and a tool that
starts another agent. Skills come first because `/goal` is a natural thing to
ship as one.

| # | Item | Size | Notes |
|---|---|---|---|
| 22 | **Agent skills: `SKILL.md` discovery and progressive disclosure.** | **M** | A skill is a directory with a markdown file whose frontmatter names it and says when to use it; the body is loaded only when it applies. PrimeIntellect's [goal skill](https://github.com/PrimeIntellect-ai/prime-agent/blob/b6ac5d014d99401b55820835a4966584271e9a3c/packages/coding-agent/skills/goal/SKILL.md) is the reference shape. The pieces are a loader (a user directory under `<nanus home>/skills/`, optionally a workspace one), a frontmatter parser, and a way to reach the body: either a `skill` tool — which collides with the seven-tool invariant and would need a design note — or a prompt section, which is where the domain's unused `PromptBuilder` already points. A skill read from the workspace is untrusted input, as [SAFETY.md](../SAFETY.md) says of anything that reads files. Grows to **L** if a packaged core library and a tool are both wanted. |
| 23 | **Secret storage: an OS keychain instead of `DEEPSEEK_API_KEY`.** | **L** | Move the provider key out of the environment and into the platform store — Keychain on macOS, Secret Service on Linux, Credential Manager on Windows — behind a new `SecretPort` in `nanus-ports` and a keyring adapter, with `nanus auth set` / `clear` / `status` and the `nanus config` presence line it already prints. The environment variable stays as a fallback for CI and containers. The hard part is the service: a detached `nanus service` may run with no unlocked keychain and no session bus, so the design needs a defined fallback (a `0600` file under `NANUS_HOME`) or a fail-closed refusal, and the key must never reach the config, a log, or `Debug` — guarantees the current design already keeps. |
| 24 | **A goal: a durable objective that continues across turns (`/goal`).** | **L** | A session-scoped completion contract: one objective per session, persisted in the log, with a phase (`active` / `paused` / `blocked` / `complete`), a budget, and evidence-based completion. Three surfaces, which can land in stages: the state as a durable `goal/change` session event folded by the domain (the log is already the only source of model history); model tools to read, edit, and complete it; and a human `/goal` that needs a new link request, because the interface cannot mutate a session it does not own. Automatic continuation is an agent-side driver that queues one turn while the session is idle, bounded by the goal budget *and* the existing turn budget. See [the goal research note](goal-research.md). The largest of the three and the one with the most decisions left open; possibly **XL** if it lands whole. |
| 25 | **An `agent` tool: spawn a sub-agent to do a task.** | **XL** | The model calls it with a `prompt`, a `provider`, a `model`, and a `reasoning_effort`, and gets the child's answer (or a handle) back; with a handle it can send follow-ups and check the child's progress on demand, without pulling a whole transcript into its own context. It is more than an eighth tool: it composes a second agent at runtime, with its own session, policy, and budget, and a channel the parent and child can talk over. Depends on a provider factory (item 26) and per-request effort (item 11). See [the detail below](#the-agent-tool-in-more-detail). |

### The `agent` tool, in more detail

The argument list is what makes the tool more than a second turn: the choice of
agent travels with the call rather than with the process.

```text
agent(prompt, provider?, model?, reasoning_effort?)
```

- **`prompt`** is the task, and the child's answer comes back as the tool result
  the parent reads — the same shape as any other tool, so the parent's loop needs
  no new vocabulary for the common case.
- **`provider`, `model`, and `reasoning_effort`** select the child's model per
  call. `provider` is why this is a runtime composition rather than a second
  `Harness`: the process builds one adapter from configuration today
  (`build_llm`), so a per-call choice needs a provider factory keyed by name —
  which is what makes the second-provider and runtime-switching items
  prerequisites rather than siblings.
- **The bounds** are not optional. A child needs its own step and token budget,
  the tree needs a depth limit and a cap on children, and the parent's budget has
  to account for what its children spent. A parent that can spawn without a bound
  has rebuilt the cost hazard the turn budget exists to prevent.

Two communication shapes are worth keeping distinct:

- **Synchronous** — the tool call returns when the child finishes. No mid-flight
  channel, and the simplest thing that is still useful: "go and find this out".
- **A child with a mailbox** — the call returns an id, and the parent can send a
  follow-up, check the child's progress, wait, or cancel. Delivery belongs at the
  child's turn boundary and never into the middle of one, exactly as the link
  refuses a prompt to a busy session. The child is a session, so its transcript is
  already the observation channel; a follow-up is just the next prompt, and it
  should be recorded as coming from the parent rather than from a person.

The questions to settle before the code are the policy ones: whether a child
inherits the parent's sandbox and approval state (it should), whether a prompt
the user cannot be shown from inside a child denies the call (it must, fail
closed), whether a child may itself call `agent` (a depth limit, or a flag), and
how a child's session is linked to its parent so `nanus sessions` can show a
tree. Child progress also has nowhere to appear yet: the link's frames are flat,
so either a child's turn is not streamed to the parent's client or the frame
vocabulary grows a way to nest one.

### Following a child's progress

The point of reading a child is to know whether it is making progress or stuck,
not to replay its work — and the parent's context is the scarce resource, so the
read is a deliberate check-in, never a stream. Nothing from a child reaches the
parent automatically. The parent asks, on its own cadence, and only what it asks
for is charged to its context; there is no timer that pushes a child's output
into the parent, and no check-in happens at all unless the parent makes one.

Two reads are worth having, and they are different sizes:

- **A progress digest, for the question "is it stuck?"** A small, bounded summary
  of the child's current state: whether it is running, idle, or finished, how many
  steps it has taken, how long it has run, what it has spent, the newest thing it
  said, and the last few tool calls with their outcomes. It is a pure fold of the
  child's session events, like the transcript fold but pointed at the present
  rather than the whole history — the same idea as `Ctrl+T` summarising a run of
  tool calls, computed in the bundle so it does not need the interface crate.
  This is what a periodic check-in uses, and it should fit in a few hundred
  tokens regardless of how long the child has been running.
- **A transcript window, for the question "what did it actually do?"** A tail, or
  everything since a cursor, bounded by a count — the convention `read` already
  uses. This is the explicit request, and it is the read that can grow, which is
  exactly why it is not the default.

**Progressive by cursor.** A check-in returns only what has happened since the
parent last looked, so polling a long-running child costs the new steps rather
than a replay from the start. A parent that ignores the cursor and asks for the
whole transcript is choosing to pay for it, and the count cap still bounds the
answer.

The digest is also where "stuck" should be named rather than left for the parent
to infer: repeated identical tool calls, a run of failing results, no new step
between two check-ins, a budget nearly exhausted, or a turn that ended without
the objective. Saying so is what lets the parent decide to send guidance, cancel,
or take the work over — the decision the read exists to inform.

Three constraints shape the read itself:

- **The unit is the child's own session events**, because that is what the child
  writes and what outlives it: a parent that reads a finished child reads the
  same log `nanus tui --session` would. The digest is a fold over those events
  rather than a second source of truth.
- **Reading must not contend with the child's turn.** A turn owns the log while
  it runs, which is why the link server caches what a listing shows rather than
  borrowing the session. Progress therefore has to come from a snapshot the
  child publishes as it appends, or from reads between its steps — never from a
  second borrow of a session a turn is writing.
- **A parent reads what it spawned, and no further.** The store has no ownership
  relation today, so a child has to record its parent for the read to be scoped
  rather than a way to read any session on the machine.

Worth stating plainly: a child's transcript is everything the child saw,
including any file or command output it read, so a parent that pulls a window of
it into its own context is choosing to send that to its model. Bound the window
with a count, as every other read here is bounded — and prefer the digest when
the question is only whether the child needs help.

## Later: larger bets

| # | Item | Size | Notes |
|---|---|---|---|
| 26 | **A second model provider.** | **M** | The `LlmPort` seam is real and an OpenAI-compatible adapter is mostly request and response encoding; a genuinely different protocol is more like an **L**. Nothing in the tools or the domain should change. |
| 27 | **An OS-enforced sandbox.** | **XL** | Confinement is advisory: the tools refuse or confine writes, but an approved program can do anything the user can, including reach the network. Landlock or seccomp on Linux and `sandbox-exec` on macOS, with the platform and `unsafe` story written down first. |
| 28 | **Network confinement.** | **XL** | Part of 27, and separable only if 27 lands as a mechanism with more than one policy. |
| 29 | **Windows support.** | **XL** | The link is a Unix domain socket and the shell adapter depends on `nix` for process groups, so this is a transport plus a process-lifecycle story, not a build flag. |
| 30 | **Automated end-to-end tests for the interface.** | **M** | Raw mode needs a real terminal; a PTY-backed test binary would cover the alternate screen and the drawing that are manual today. |
| 31 | **Background tasks (`Ctrl+B`).** | **L** | Needs a task model the agent and the session own, not just a key in the interface. |

## By design, not planned

These appear on other projects' roadmaps. They are absent here on purpose, and
the [design decisions](design.md) say why:

- **A policy that means yes to everything** was on this list, and is now shipped as the
  explicit `all_calls` state (and the session-scoped "always allow" answer). It is never
  the default, the status line names it, and
  [design.md](design.md#approval-is-a-three-state-axis-fail-closed-at-the-default) argues
  the reversal.
- **Aliases for retired model ids.** `deepseek-chat` and `deepseek-reasoner` do
  not resolve, because silently mapping a retired name onto a new model changes
  a user's output without saying so.
- **A remote link.** The socket is local, unauthenticated, and trusts processes
  running as the user; there is no remote mode to secure, and
  [SAFETY.md](../SAFETY.md) asks that one not be added by forwarding the socket.
- **An in-process interface.** The core not linking the interface is the
  structural decision that keeps `nanus run` small; the socket is the price.
- **A larger core toolset.** Seven tools is the design. A tool earns its place
  by being a mechanism the shell cannot provide as well, and convenience
  wrappers are not it.
- **The API key in the configuration file.** It is read from the environment on
  use and has no field to live in.
- **An image fetch or a URL request from the markdown renderer.** The view is a
  pure function of the transcript; a remote fetch on a model's say-so is a
  request the reader did not ask for.
- **A worker pool or parallel turns.** The kernel is single-threaded and its
  futures are deliberately `!Send`; concurrency is cooperative, and item 3 is
  the extent of the parallelism planned.
