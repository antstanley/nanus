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
numbers are the plan's, kept because later items refer to them. Number 16 was the one number the plan
had left unfilled — an item that was taken out and whose number was not reused — and it now names
selecting and copying, which was asked for once the rest of this block had landed. Nothing below it
moved, so every reference from 17 onwards still means what it said.

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
| 16 | **Selecting and copying text out of the transcript.** A drag with the mouse or `Shift` with a movement key selects rendered rows, `Ctrl+C` copies them, and `/copy` takes the newest answer without pointing at it. The clipboard is the platform's own tool with the terminal's `OSC 52` behind it, and the two are reported differently because only one of them can be confirmed. A copy that worked clears the selection. (`Ctrl+C` was later made copy-only — `Esc` is the stop key and `Ctrl+Q` quits — and a drag over anything that is not the transcript selects the cells it crossed, so a dialogue can be copied from too.) | `nanus-tui/src/{copy,view,runtime,command}.rs` |

## Shipped: the provider seam and the secret store

Items 23 and 26, plus the thing that made 26 more than a second adapter: providers are
*selected*, not compiled in, so the answer to "can nanus talk to z.ai" is a configuration
line rather than a fork.

| # | Item | Where it landed |
|---|---|---|
| 23 | **Secret storage.** A `SecretPort` and a `SecretHandle` in `nanus-ports`, and a chain of stores in `nanus-adapter-secret`: the macOS keychain (through `/usr/bin/security`, the platform's own tool), a `0600` file under `<nanus home>/secrets/`, then the provider's environment variable. A read takes the first value any store can produce, so a locked keychain on a detached service does not hide a variable the service was started with; a write goes to the first store that will take it, so `nanus auth set` prefers the keychain and falls back to the file without the caller choosing. The store is pluggable in both directions: `SecretPort` is the port an adapter implements and `SecretBackend` is one store inside the chain, so another platform is an implementation plus a line. **macOS is the only platform store that ships** — Linux Secret Service and the Windows Credential Manager are the same trait and no implementation. The one exposure is stated rather than hidden: `security add-generic-password` takes the value as an argument, so for the few milliseconds that process lives the value is in its argument list; `SAFETY.md` says so. `nanus auth set` reads from standard input, never an argument. | `nanus-ports/src/secret.rs`, `nanus-adapter-secret/`, `nanus-cli/src/cli.rs`, `SAFETY.md` |
| 26 | **A second model provider — and a provider table.** The plan called for one more adapter; what landed is a table in `nanus-bundle/src/provider.rs` that names each provider, its credential variable, its plans, the models it offers, and the ceiling it refuses to exceed, plus a `Selection` that resolves *what a configuration means*: provider, plan, endpoint, model. Every one of those is optional in the schema and absent means "the provider's own answer applies", so `provider = "openai"` alone gets OpenAI's host and model, and a `model` defaulted to a DeepSeek id can never be run against another provider. Three adapter crates carry it: `nanus-adapter-deepseek` (unchanged in behaviour), `nanus-adapter-openai` (OpenAI and z.ai, two wires and a vendor table), and `nanus-adapter-anthropic` (the Messages API, which is not chat completions). A plan is an endpoint plus a default model, which is what z.ai's coding subscription is; OpenAI's ChatGPT subscription tier was **listed and refused by name** with its reason until its wire landed, and it is now a plan that authorizes over OAuth and speaks the Responses API. Anthropic's extended thinking is not requested at all, and the adapter reports no effort rather than a plausible one. | `nanus-bundle/src/{provider,compose}.rs`, `nanus-adapter-{openai,anthropic}/`, `nanus-adapter-config/src/config.rs` |

The two items the plan had left open in this area are recorded where they belong rather
than being quietly dropped: OpenAI's subscription plan is in the table and authorized over
OAuth rather than refused, and the second platform secret store is a `SecretBackend`
implementation nobody has written.

## Shipped: sessions and the link

All five items of the session work, taken in the order they were worth doing. The numbers are the
plan's, kept because later items refer to them (item 25 depends on the provider table, item 22 on
the prompt sections that item 21 touched).

| # | Item | Where it landed |
|---|---|---|
| 17 | **A client attaching mid-turn catches up.** The turn in flight is the one thing a client cannot read anywhere else — the store is written when a turn *ends* — so the agent keeps, per held session, exactly the frames of the running turn that the log does not have yet, and hands them to a client that attaches in the middle of one. They cross as a single `Backlog` frame right after `Attached`, which is what makes the batch atomic: one item on the connection's queue, so no live frame can slip inside it and draw the newest delta before the text it continues. The snapshot and the viewer's registration happen in one synchronous region, so every frame is either in the batch or in the live stream and never in both. The batch is folded where folding changes nothing (adjacent deltas joined, which bounds it by segments rather than tokens) and emptied in the same instant the turn reaches the log, so a client sees a finished turn in the store and a running one in the batch, never both and never neither. A question the turn is waiting on travels the same way, because it is state rather than history: a client arriving after it went out is shown it, with the reason the first client was given, and can answer it. `PROTOCOL_VERSION` moved to 3, because an added frame variant is a decode error for an older peer rather than something it can ignore. | `nanus-link/src/{protocol,server}.rs`, `nanus-tui/src/runtime.rs`, `docs/sessions.md` |
| 18 | **A rename reaches a held session.** The agent's copy of a name is a cache of what the store says, and `nanus sessions name` writes the store without connecting to the link, so the cache could be stale for the life of a service. Every report the agent makes about a held session now re-reads the name first — a listing, and the attachment that labels a client's screen — and a name is **resolved** through the store rather than against that cache, so `--resume` on a name that has moved on cannot join the conversation it used to belong to. Only an id is answered from memory, because an id is a store key rather than an alias. | `nanus-link/src/server.rs`, `docs/sessions.md` |
| 19 | **A session is claimed for writing, so a second writer is refused.** A writer claims the session it holds — a `nanus run` for the length of its run, an agent for as long as it holds the session, and *not* a client attaching to one — and a second writer is refused with a sentence naming the holder and what to do instead. The claim is a file beside the log, locked with the operating system's own `flock` and labelled with the holder's pid and a word for what it is. The lock is what decides a race between two processes — it is atomic, and only one of them takes it — and the kernel releases it when the holder exits, however it exits, so a claim a crashed writer left behind is not a state to be detected and taken over but a lock that is already gone. Releasing it happens in a `Drop`, which is why it is a field of the held session rather than something a caller remembers — a claim that outlived its holder would refuse the next writer for the life of the process. A `StorePort` release is synchronous for exactly that reason: a `Drop` cannot await. | `nanus-ports/src/store.rs`, `nanus-adapter-store/src/store.rs`, `nanus-link/src/server.rs`, `nanus-cli/src/cli.rs`, `docs/sessions.md` |
| 20 | **Names: one word, and case does not make a second one.** The decision the item asked for, taken and written down where it belongs. **No namespaces**: a name is an alias for one store key, the store is one flat directory, and a `/` in a name would suggest a tree that does not exist — a user who wants grouping writes it into the name, because punctuation is part of the word rather than a level. **Case-insensitively unique, case-preserving**: naming a session `Nightly` when `nightly` is held is refused and names the session that holds it, resolving either spelling finds it, and what is stored is the spelling a session was named with, so a rename is how a name changes case. Leading and trailing whitespace is trimmed, because a name nobody can see is one nobody can type back. A store that somehow holds two names folding to one answers with the same session every time rather than at a directory listing's mercy. | `nanus-ports/src/store.rs`, `nanus-adapter-store/src/store.rs`, `docs/sessions.md` |
| 21 | **Manage the context window.** Prompt assembly replays the whole log, so a long session eventually asks for more tokens than the model has. The policy is in `nanus-domain`'s `context` module and is deliberately two things: **deterministic** (the same messages and the same budget always produce the same prompt, because a session exists to be comparable) and **visible** (the model reads a notice at the gap, and the reader is told through the CLI and the interface). The unit dropped is a **whole turn**, oldest first, so a tool call can never be separated from the result answering it — which a provider refuses outright — and neither the system prompt nor the newest turn is ever dropped. A prompt that cannot be shortened enough is **refused** with a sentence naming `context_budget`, because the alternatives are a provider refusing the request with a message about the request, or a turn quietly answering from half a conversation. There is no tokenizer and there will not be one: the estimate is characters over four plus a per-message cost, which is why the default budget sits below every provider's window, and the provider's own usage report is the real number to check a budget against. | `nanus-domain/src/{context,agent}.rs`, `nanus-bundle/src/{agent_loop,error}.rs`, `nanus-adapter-config/src/config.rs`, `nanus-cli/src/progress.rs`, `nanus-link/src/{protocol,server}.rs`, `nanus-tui/src/runtime.rs` |

## Shipped: the goal

Item 24 is the largest thing on the plan and lands in pieces. Three of them have landed — the
durable state, the human `/goal` lifecycle, and the model tools — and the fourth, automatic
continuation, is deliberately absent: it is the part that can run away, so the cost hazard the
turn budget exists to prevent does not exist yet.

| What landed | Where it lives |
|---|---|
| **A goal is durable session state.** A `Goal` in `nanus-domain` carries an objective, a phase (`active` / `paused` / `complete` / `abandoned`), a revision, and when it changed. The domain refuses a blank or oversized objective; every transition bumps the revision and is idempotent when it would change nothing, so a pause of an already-paused goal records no event. `complete` and `abandoned` are *terminal* — achieved and given up on are answers, not stages — so neither can be paused or resumed, and the way to start the next piece of work is a new objective. It is persisted as a `goal/change` session event — the whole goal after a change, or its absence after a clear — and folded from the log by `SessionLog::goal`, so a resumed session knows its objective without any other source, and a client cannot set one behind the log's back. Like a turn boundary, it never reaches a model. There is deliberately no `blocked` phase: a goal that cannot proceed *now* is paused with a note saying why, and one that cannot be achieved at all is abandoned with the same. | `nanus-domain/src/goal.rs`, `session.rs` |
| **The human command crosses the link.** `/goal` is the first slash command that must reach the agent, because the interface cannot mutate a session it does not own. A `Goal { action }` request and a `Goal { goal }` frame cross the link — `PROTOCOL_VERSION` moved to 8 for the added variants — and the agent writes the change through the same store a turn is recorded through: it builds and saves the updated session *before* it swaps in the in-memory copy, so a refused write leaves the goal unchanged rather than applied and lost. A change while a turn runs is refused by name, because the turn holds the session, and a change *reserves* the session for the whole of its write, so a prompt arriving mid-write is refused rather than starting a turn over it; a status read is answered from the cache even mid-turn. An attachment is shown a goal the session already has, from a cache kept beside the session rather than borrowed from it, so a client joining mid-turn still sees the objective. | `nanus-link/src/{protocol,server}.rs` |
| **The interface, and the recording.** `/goal` reads the goal, sets an objective, and moves the lifecycle; a goal frame becomes the interface's own notice, and a recording folds the same line out of its log so a re-read transcript reads as the watched one did. | `nanus-tui/src/{command,runtime,replay}.rs`, `docs/tui.md` |
| **The model acts on it.** Five tools — `get_goal`, `create_goal`, `update_goal`, `pause_goal`, `abandon_goal` — let the model read the objective, set one when the user asks for it, change it, suspend it, and give up on it. They are *not registered*, and the split is structural: their effect is a `goal/change` record in the session log, and a tool executor is `'static` and cannot borrow the session a turn holds, so the loop runs them itself. The two lists of offered tools meet in one place — the runner, which sends the schemas and advertises the count — and a test asserts no name is in both. The authority is the model's to exercise but not to exceed: `complete` requires `evidence` of what was checked, `abandon` requires a reason and is terminal (a new phase, `abandoned`, distinct from `complete`), and **clearing** a goal stays the person's — `/goal clear`. | `nanus-bundle/src/{goal_tools,agent_loop}.rs`, `nanus-domain/src/goal.rs`, `docs/features.md` |

Still open from the same item: **automatic continuation** — an agent-side driver that queues one
turn while the session is idle, bounded by a goal budget *and* the turn budget, and disarmed on
resume until a person re-arms it. It is deliberately last, because a driver reintroduces exactly
the unattended-loop cost hazard the turn budget exists to prevent and needs a goal budget first.
See [the goal research note](goal-research.md).

## Next: new capabilities

Two additions larger than a feature but short of a rewrite: a way to package behaviour, and a
tool that starts another agent. Skills come first because `/goal` is a natural thing to ship as
one. (A fourth, a way to hold a secret, has shipped; see
[above](#shipped-the-provider-seam-and-the-secret-store). A third — the goal — has shipped
everything but its continuation driver; see [the goal](#shipped-the-goal).)

| # | Item | Size | Notes |
|---|---|---|---|
| 22 | **Agent skills: `SKILL.md` discovery and progressive disclosure.** | **M** | A skill is a directory with a markdown file whose frontmatter names it and says when to use it; the body is loaded only when it applies. PrimeIntellect's [goal skill](https://github.com/PrimeIntellect-ai/prime-agent/blob/b6ac5d014d99401b55820835a4966584271e9a3c/packages/coding-agent/skills/goal/SKILL.md) is the reference shape. The pieces are a loader (a user directory under `<nanus home>/skills/`, optionally a workspace one), a frontmatter parser, and a way to reach the body: either a `skill` tool — which collides with the seven-*registered*-tool rule and would need a design note, though the [goal tools](#shipped-the-goal) are now the precedent for a tool the loop runs itself rather than registering — or a prompt section, which is where the domain's unused `PromptBuilder` already points. A skill read from the workspace is untrusted input, as [SAFETY.md](../SAFETY.md) says of anything that reads files. Grows to **L** if a packaged core library and a tool are both wanted. |
| 24 | **A goal: the continuation driver.** | **L** | The state, the human `/goal` lifecycle, and the model tools — `get_goal`, `create_goal`, `update_goal`, `pause_goal`, `abandon_goal` — [have landed](#shipped-the-goal). What remains is the part that can run away: an agent-side driver that queues one turn while the session is idle, bounded by a goal budget *and* the existing turn budget, and disarmed on resume until a person re-arms it. A goal budget must arrive before the driver, because the driver reintroduces exactly the unattended-loop cost hazard the turn budget exists to prevent. See [the goal research note](goal-research.md). |
| 25 | **An `agent` tool: spawn a sub-agent to do a task.** | **XL** | The model calls it with a `prompt`, a `provider`, a `model`, and a `reasoning_effort`, and gets the child's answer (or a handle) back; with a handle it can send follow-ups and check the child's progress on demand, without pulling a whole transcript into its own context. It is more than an eighth tool: it composes a second agent at runtime, with its own session, policy, and budget, and a channel the parent and child can talk over. Both prerequisites have landed — per-request effort (item 11) and the provider table (item 26) — so what is left is the child's lifetime, its budgets, and the read a parent makes of it. See [the detail below](#the-agent-tool-in-more-detail). |

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
- **A larger core toolset.** Seven *registered* tools is the design. A tool earns
  its place by being a mechanism the shell cannot provide as well, and convenience
  wrappers are not it. (The five goal tools are not an exception to the rule but an
  application of it: no shell can mutate a session's durable objective, and they are
  the loop's own rather than the registry's — see
  [the toolset](features.md#the-goal-tools).)
- **A credential in the configuration file.** It lives in the secret store
  `nanus auth` writes to — the platform keychain, then a `0600` file, then the
  environment — and the configuration type has no field it could live in.
- **An image fetch or a URL request from the markdown renderer.** The view is a
  pure function of the transcript; a remote fetch on a model's say-so is a
  request the reader did not ask for.
- **A worker pool or parallel turns.** The kernel is single-threaded and its
  futures are deliberately `!Send`; concurrency is cooperative, and item 3 is
  the extent of the parallelism planned.
