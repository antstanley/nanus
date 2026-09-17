# Features

What `nanus` supports today, grouped by area. This page is a map rather than a
manual: each section links to the page that explains the reasoning and the
details. For what is incomplete or deliberately absent, see
[status](status.md#known-limits) and the closing section here; for what is
planned, and the effort attached, see [the roadmap](roadmap.md).

## The agent

One agent, with three lifetimes. What differs between the modes is how long the
agent lives and how it is reached, not what it does — there is one code path
that runs a turn.

| Mode | Lifetime | Reached by |
|---|---|---|
| `nanus run <task>` | one turn | stdout, in the same process |
| `nanus tui` / a bare `nanus` | the interface's | the local link, for a shell-scoped agent |
| `nanus service` | until stopped | the local link, for a process |

- **Headless one-shot runs**, with the answer on stdout and nothing else.
  Reasoning and tool activity go to stderr (`--verbose`), and the exit code is
  meaningful: `0` only for a completed turn.
- **An interactive interface** (`nanus tui`, alias `nanus ui`), which is its own
  binary and always a client of the agent over a local socket. See
  [the interface](tui.md).
- **A long-running service** (`nanus service`), detached or in the foreground,
  that holds sessions open across terminals. See [the service](service.md).
- **A bounded turn.** `max_steps_per_turn` (default 512) caps a turn; the model
  is told its budget, and an ending always says why it stopped — completed,
  errored, at the token ceiling, out of steps, or interrupted. Those five are what
  the harness produces; the domain's vocabulary carries two more, a policy block
  and a human abort, that nothing mints yet.
- **Interruptible turns.** `Ctrl+C` / `Esc` in the interface and `SIGINT` on a
  headless run ask the turn to stop at the next safe point.
- **A step's tool calls run together, bounded.** `max_parallel_tools` (default 4)
  caps how many are in flight at once. The concurrency is cooperative — the kernel
  is single-threaded and its futures are `!Send` — and the log still records every
  call and its result in call order, whatever order the work finished in.
- **A model-visible failure instead of a panic.** A bad tool argument is a
  failed tool result the model can correct, not a harness error.
- **Everything below the loop is a plugin.** The clock, the filesystem, the shell,
  the session log, the model adapter, and the tool registry mount on the kernel as
  services, so a different provider, toolset, store, or filesystem is a plugin rather
  than an edit to the loop — and unloading one withdraws it and deactivates what
  depended on it. The permission policy and the agent loop are *not* plugins: the
  policy is a configuration value the loop reads, and the loop is built over the
  handles the composition publishes. See [design decisions](design.md) and
  [architecture](architecture.md).

## The toolset

Exactly seven tools, and the count is the design: each is a mechanism a shell
cannot provide as well, not a convenience wrapper. See
[design decisions](design.md#seven-tools-and-the-count-is-the-design).

| Tool | What it does |
|---|---|
| `read` | Reads a file through a 1-based `offset`/`limit` line window, with line numbers, a byte ceiling, and a note on how to continue. |
| `write` | Creates or replaces a file. |
| `edit` | Replaces text, requiring `old_string` to occur exactly once unless `replace_all` is set — an ambiguous or absent match is refused rather than guessed. |
| `read_image` | Attaches a PNG, JPEG, WebP, or GIF to the conversation as an image content block. |
| `glob` | Finds files by path pattern, anchored to the workspace root (`*.rs` for the top level, `**/*.rs` at any depth), with a result cap — and a notice naming the cap when matches were dropped, rather than whenever the cap was reached. |
| `grep` | Finds text inside files, grouped by file, optionally narrowed by one `include` glob, with capped matches and truncated lines that say so. |
| `bash` | Runs a program in the workspace root unless a `workdir` says otherwise, with an optional timeout, reporting stdout, stderr, and the exit code. A non-zero exit is a result, not a failure; output is capped and truncated with a notice; the whole process group is killed so grandchildren are not orphaned. |

Only a tool's `name`, `description`, and `parameters` may reach the model; the
executable half is not serialisable, so the allowlist is carried by the types.
See [the toolset](../crates/nanus-bundle/src/tools/mod.rs).

## Model providers

- **DeepSeek**, through `nanus-adapter-deepseek`. Supported ids are
  `deepseek-flash` (the default) and `deepseek-v4-pro`; retired ids
  (`deepseek-chat`, `deepseek-reasoner`) deliberately do not resolve.
- **Streaming responses** over SSE, with reasoning content and tool calls
  reassembled from their frames.
- **Tool calling**, including the reasoning passback the API requires when a
  request carries tools, and an empty assistant turn sent as `content: ""`.
- **Usage accounting** — prompt, cached, and generated tokens, including how
  much of the generation was thinking.
- **Request controls**: `max_tokens` (default 128000) and `reasoning_effort`
  (`minimal` / `low` / `medium` / `high`, default `medium`).
- **A real provider seam.** `LlmPort` is the boundary; a second provider is one
  adapter plus a registration, and nothing in the tools or the domain changes.

## Sessions

A session is the conversation, written down as it happens. See
[sessions](sessions.md).

- **Append-only JSONL persistence** under `<nanus home>/sessions/`, with
  time-ordered UUID keys, so a listing sorts by creation.
- **Every run persists its transcript**, including a run that failed — which is
  exactly the one worth resuming.
- **Named sessions**, with `--name` on `run` and `tui`, and
  `nanus sessions name <name> <session>` to rename later. A name is an alias for
  a store key: one session has one name, and a held name is refused rather than
  moved.
- **Deleting a session**, with `nanus sessions delete <name|id>`. The reference is
  resolved the way naming resolves one, a reference that answers to nothing is
  refused rather than reported as a deletion, and the session's name goes with it.
- **Reporting on a run**, with `nanus sessions show [--json] <name|id>`: the
  configuration it ran under, the turns, steps and requests, the prompt tokens
  split into cached and read, the generated tokens and how many were thinking, a
  per-model breakdown, and why the last turn ended. Read from the log, so it needs
  no model and no key, and `nanus run --verbose` prints the same totals in one line
  on stderr when the turn finishes.
- **What produced a run is recorded.** The session header carries the model that
  was configured, the reasoning effort, the sandbox mode, the approval policy and
  the release that wrote it; each model turn carries the model and effort that
  produced it, since a session can be resumed against a different model. Absent
  means not recorded rather than a default, so a session from before a field
  existed reports the gap instead of inventing a value for it.
- **Resuming** by name or id: `--resume` on `run` and `tui`, including against a
  service that is already holding the session.
- **Live sessions.** An agent holds sessions open, a turn runs in its own task
  so it outlives the client that asked, and several clients can watch one
  session at once. One turn at a time per session; a prompt to a busy session is
  refused rather than queued — the interface holds prompts typed during a turn
  and sends them, one per turn end, when the agent is ready.
- **Reading without an agent or a key.** `nanus tui --session [<id>]` replays a
  recorded transcript from the same event log the live view uses, with
  `--scroll <rows>` to open part way back.
- **A bounded working set.** An agent holds at most 32 idle sessions and lets
  the least recently used go; a session that is running or has a client
  attached is never dropped.

## Configuration

A flat TOML file at `<platform config dir>/nanus/config.toml`, every field
defaulted, with a real `config_version` migration chain. Unknown keys are
ignored, and there is no field that can hold the API key. See
[configuration](../crates/nanus-adapter-config).

| Field | Default | Values |
|---|---|---|
| `model` | `deepseek-flash` | `deepseek-flash`, `deepseek-v4-pro` |
| `max_tokens` | `128000` | per-response budget |
| `reasoning_effort` | `medium` | `minimal`, `low`, `medium`, `high` |
| `approval_policy` | `per_call` | `per_call`, `permitted`, `all_calls` |
| `sandbox_mode` | `read_only` | `read_only`, `workspace_write`, `danger_full_access` |
| `max_steps_per_turn` | `512` | steps in one turn |
| `max_parallel_tools` | `4` | how many of a step's calls may be in flight at once |
| `tui_detail` | `compact` | `compact`, `full` |
| `markdown` | `true` | render the model's answers as markdown |
| `mermaid` | `true` | draw `mermaid` fences as text diagrams |
| `system_prompt` | built-in | override the system prompt |
| `workspace_root` | the current directory | root the tools are confined to |
| `service_socket` | `<nanus home>/run/agent.sock` | where a service listens |
| `service_log` | `<nanus home>/nanus-service.log` | where a detached service writes |
| `config_version` | the build's version | the schema the file was written with; a newer one is refused, and a missing one is read as the pre-1.0 shape and migrated |

Environment variables:

| Variable | Effect |
|---|---|
| `DEEPSEEK_API_KEY` | Provider key. Read on use; never stored, serialised, or rendered. |
| `NANUS_CONFIG` | Override the configuration file path. |
| `NANUS_HOME` | Override the session-store home (and the default socket and log paths). |
| `NANUS_TUI` | Override the path to the interface binary. |
| `NO_COLOR` | Render with no colour at all, keeping bold and italic. |
| `RUST_LOG` | Tracing filter for the service and core logs. |

## The command line

Global options on `nanus`: `--verbose`, `--quiet`, `--config <PATH>`. `--quiet` is
accepted and does nothing, because the default already is what it asks for — stdout
carries the answer and nothing else on every run — and it conflicts with `--verbose`,
which asks for progress on stderr.

| Command | What it does |
|---|---|
| `nanus run [--name NAME \| --resume NAME\|ID] <TASK>` | One prompt, one answer on stdout, then exit. |
| `nanus tui` / `nanus ui` | Start the interface against a shell-scoped agent. |
| `nanus tui --connect [--socket PATH]` | Talk to a `nanus service` instead. |
| `nanus tui --resume REF` / `--name NAME` | Open or record a particular session. |
| `nanus tui --session [ID] [--scroll ROWS]` | Read a recorded transcript; no key needed. |
| `nanus service start [--foreground] [--socket PATH] [--log PATH]` | Start a service, detached by default. |
| `nanus service stop [--socket PATH]` | Ask a running service to stop. |
| `nanus service status [--socket PATH]` | Report whether one is running, and which sessions it holds; non-zero when nothing answers. |
| `nanus config` | Print the effective configuration; no key needed. |
| `nanus sessions` | List recorded sessions, newest first; no key needed. |
| `nanus sessions name <NAME> <SESSION>` | Record or change a session's name. |
| `nanus sessions delete <NAME\|ID>` | Remove a session and release its name. |
| `nanus sessions show [--json] <NAME\|ID>` | Report what a session ran under and what it spent; no key needed. |

A bare `nanus` starts the interface when there is a terminal and prints usage
when there is not. The usage text is built from the same parser the commands
are, so help and grammar cannot disagree.

## The interface

The view layer is testable without a terminal — a pure function of a transcript
and an input buffer — and the raw-mode loop is exercised by hand. What it
supports:

- **A live conversation** against a shell-scoped agent, a service, or a
  transcript being replayed. Recorded and live transcripts are built by the
  same renderer from the same event log.
- **Key bindings** modelled on Claude Code's interactive mode: submit, multi-line
  prompts (`Alt+Enter`, `Shift+Enter` where the terminal reports it, `Ctrl+J`),
  cursor movement, word and line deletion, kill/yank, history browsing, and
  reverse search (`Ctrl+R`). See [the key table](tui.md#keys).
- **Mouse support**: the wheel scrolls, and a click in the composer places the
  caret.
- **Follow and scroll-back**: the view follows new output until you scroll away
  and follows again at the bottom, with `PageUp`/`PageDown` and the wheel.
- **A growing multi-line composer** with word-boundary wrapping and a caret drawn
  as a cell style rather than a glyph.
- **One-line, summarised machinery**: tool calls and the newest line of thinking
  are compact by default; `Ctrl+O` or `tui_detail = "full"` shows whole argument
  blocks and reasoning segments, and `Ctrl+T` / `Ctrl+E` fold runs of them away.
- **Markdown answers**: headings, emphasis, inline code, links, fenced code
  blocks — highlighted for the handful of languages a coding answer is written in —
  ordered/unordered/task lists, blockquotes, horizontal rules, pipe tables, images
  (as placeholders), and TOML frontmatter stripping. Only the model's answer is
  parsed; reasoning and tool output are verbatim.
- **Mermaid as text diagrams**: flowcharts, sequence, pie, gantt, state, class,
  quadrant, and block diagrams, falling back to the fence source when a diagram
  does not parse.
- **Live session figures**: last and average token rates, time to first token,
  cache hit share, and a `/stats` breakdown.
- **A session summary on exit.** Leaving the interface prints a table of what the
  session did and what it spent: the model and permission state it ran under, the
  turns, steps and requests, the prompt split into cached and read, the generated
  tokens and how many were thinking, and the rates and waits this run measured.
  Read from the log and from the interface's own measurements respectively, so the
  session's totals cover turns that ran before this interface opened. See
  [the interface](tui.md#commands).
- **Slash commands**: `/exit`, `/quit`, `/stats`, `/help`, `/clear`, and `/model`.
  Anything else is named in the transcript rather than sent to the model.
- **The key list on screen** (`?`, or `/help`), scrolled from one table so a
  binding cannot be documented in one place and forgotten in another.
- **The settings a key changes**: the approval state (`Shift+Tab`, in a dialog that
  says what each state grants), the model (`Alt+P`, or `/model <id>`), and the
  reasoning effort (`Alt+T`). Each is drawn where a reader can see which is in
  force.
- **`@` file mentions**, completed from the workspace by `Tab` and expanded to the
  path — the model reads the file with a tool rather than being handed its
  contents.
- **Pasting an image** (`Ctrl+V`): the clipboard is read by the platform's own
  tool, the bytes are checked by their magic number, and the file is written inside
  the workspace so `read_image` can reach it.
- **`!` for your own commands**: a line that opens with `!` is a shell command the
  interface runs itself, echoed into the transcript. It is not the model's, is not
  recorded, and is not confined — see [SAFETY.md](../SAFETY.md).
- **No colour leaks**: `NO_COLOR` switches to a monochrome theme that keeps
  modifiers, so the caret and bold/italic still render. The markdown renderer
  does no I/O and strips control characters at the parse boundary.

## The service

An agent that outlives the shell, reachable from anything on the machine that
runs as you. See [the service](service.md).

- **`start` / `stop` / `status`**, with `--foreground` for a supervisor and a
  detached default that survives the shell.
- **Startup that reports failure.** `start` waits for the agent to answer on its
  socket and reports the log path when a detached daemon fails.
- **A second guard.** Starting a service where one is already listening is an
  error, not a second daemon that cannot bind.
- **Clean shutdown** by request, `SIGTERM`, or `SIGINT`, with the socket removed.
- **Custom socket and log paths**, and therefore more than one service per
  machine.
- **Held sessions** listed by `status` — busy or idle, attached viewers, event
  count — without opening any that were not already held.

## The link

The only thing the core and the interface share. See
[the link](tui.md#the-link) and [the frames](sessions.md#what-travels-over-the-link).

- **A Unix domain socket** in `<nanus home>/run/`, `0600` inside a `0700`
  directory, with one line of JSON per frame in both directions.
- **A versioned handshake.** The agent says which link protocol version it speaks, and a
  client built from different sources refuses it with a sentence naming both rather than
  misreading a frame. An unversioned handshake reads as version zero and is refused too.
- **A small frame vocabulary**: handshake, attachment, held-session listing, a
  question or prompt, the progress of a turn (text, reasoning, step boundaries,
  tool call and result, usage), an approval question and its answer, the ending and
  its reason, and interrupt, status, and shutdown requests.
- **A tool call and its result are identifiable.** Both tool frames carry the call's
  id, so a client pairs them by identity rather than by the order two frames happened
  to arrive in — a step sends every call before any result, and its results arrive in
  the order the tools finished. A frame from an agent too old to send one leaves the
  client to pair by name and order, which is what it did before the field existed.
- **Local only by construction.** No port, no TLS, no remote mode; the
  reachable set is processes already running as the same user.
- **The session log is the authority.** The link carries what happened; history
  a client shows is read from the store.

## Composition and the kernel

The Cordis-style kernel is the framework underneath. See
[architecture](architecture.md#how-the-two-halves-fit).

- **Revertible effects**: every registration records its inverse, and unloading
  a plugin reverts in reverse *activation* order — not reverse declaration order, which
  differs as soon as a plugin is written above something it depends on. A withdrawal
  also waits for the deactivations it causes, however deep the chain, so a dependent's
  teardown still resolves what it borrowed. `tests/composition.rs` asserts the trace
  order rather than the end state.
- **Reactive coeffects**: a component declares the services it needs and is
  activated when they appear and deactivated when they vanish, so load order is
  a dependency rather than a boot script.
- **A typed service registry, typed events with several dispatch modes, and a
  plugin lifecycle**, with `nanus-kernel` documented as a standalone library.
- **Composition staged in two phases** (`compose(...).await`, then
  `Pending::start()`), enforced by types, because the kernel drives hooks with
  `block_on` and cannot do so inside a runtime.

## Safety and verification

- **`unsafe` is forbidden** in every crate and at the workspace level; there is
  none in the repository. See [design decisions](design.md#safe-rust-because-the-model-is-writing-the-code).
- **No `panic!`, `unwrap`, `expect`, `todo!`, or `dbg!` in production code**;
  `assert!` is the sanctioned invariant. See [style](style.md).
- **Fail-closed approval at the tool boundary.** A tool declares what it can touch —
  read, write, or run a program — and `sandbox_mode` permits some of that outright.
  A call outside that standing permission needs an exception: `per_call` puts it to
  an answerer and denies it when there is none, `permitted` grants the ones that
  cannot destroy anything (and destructive ones aimed only at a temporary
  directory), and `all_calls` grants every exception. The default is `per_call`, so
  a harness that cannot obtain an answer denies; `ApprovalOutcome` allows only
  `AllowedOnce`; and a denial is a tool result the model can read rather than a
  dropped call. See [SAFETY.md](../SAFETY.md).
- **A standing "always allow" answer.** An approval dialog offers to record the
  tool for the session, so a person answering the same question for the fourth
  time can say yes once for the rest of the conversation. The record is per session
  and in memory, so it is not written to the log and does not outlive the agent.
- **Someone to ask, wherever a person is watching.** `nanus run` prompts on the
  terminal when stdin is one, and the interface draws a dialog over the
  conversation and answers the agent over the link. Both deny when nobody is
  there to answer, and an unattended service with no client attached is nobody.
- **The model is told what it is running under.** The system prompt carries a
  runtime section: the workspace root, the model, the approval policy, and the
  sandbox mode.
- **A sandbox mode is reported, not OS-enforced** — it governs whether writes
  are refused or confined by the tools, not what an approved program may do. See
  [SAFETY.md](../SAFETY.md) and [status](status.md#known-limits).
- **Secrets never reach a log or a request body**: the API key is read from the
  environment, is absent from the configuration type, and is redacted in
  `Debug`.
- **Four quality gates** — `cargo fmt`, `cargo clippy`, `cargo nextest`, and the
  doctests — with the tests that matter most and the bugs verification found in
  [testing and verification](testing.md).

## Not supported yet

The honest list lives in [status](status.md#known-limits); the headline items:

- **DeepSeek is the only provider.** The `LlmPort` seam is real, but no second
  adapter exists.
- **The link is Unix-only and local.** No remote mode, no Windows, no
  authentication — there is no remote mode to secure.
- **The sandbox is not OS-enforced.** Nothing confines an approved program's
  writes, its network access, or its process table.
- **Sessions are not locked.** Two writers on one log can lose a turn; attaching
  to a live session is the supported way to share one.
- **No remote fetch from the renderer**: a fenced block is highlighted by a lexer
  inside the view, and an image is a placeholder or a pasted file, never a URL
  fetched on a model's say-so.
- **The interface's `!` command is not recorded and not confined.** It is your
  shell rather than the agent's: see [SAFETY.md](../SAFETY.md).
