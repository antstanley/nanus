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

| Mode                         | Lifetime        | Reached by                               |
| ---------------------------- | --------------- | ---------------------------------------- |
| `nanus run <task>`           | one turn        | stdout, in the same process              |
| `nanus tui` / a bare `nanus` | the interface's | the local link, for a shell-scoped agent |
| `nanus service`              | until stopped   | the local link, for a process            |

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
- **A bounded prompt.** Every step replays the whole log, so a long session
  eventually exceeds the model's window. `context_budget` (default 64000 estimated
  tokens) is the ceiling for one request: past it the _oldest turns_ are dropped,
  whole, with a notice the model reads where the gap is, and the reader is told —
  the CLI on stderr and the interface as a notice in the transcript. A turn whose
  _newest_ part does not fit is refused with a sentence naming the field rather
  than sent to a provider that would refuse the request. The estimate is
  characters over four plus a small per-message cost, deliberately approximate:
  there is no tokenizer here, the provider reports the real count with every
  response, and the default sits well below every provider's window. See
  [context fitting](../crates/nanus-domain/src/context.rs).
- **Interruptible turns.** `Esc` in the interface and `SIGINT` on a
  headless run ask the turn to stop at the next safe point. (`Ctrl+C` is the copy
  key and never stops anything.)
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
  depended on it. The permission policy and the agent loop are _not_ plugins: the
  policy is a configuration value the loop reads, and the loop is built over the
  handles the composition publishes. See [design decisions](design.md) and
  [architecture](architecture.md).

Library hosts use `nanus-bundle` without default features, supply trusted prompt text and
ports, and register ordinary tool executors. `with_tool_policy` adds exact-call checks;
`run_turn_with_control` races a sticky caller signal against idle model, policy, approval
and tool futures. Cancellation settles unfinished calls once and closes the turn; the
host remains responsible for stopping detached processes when their futures are dropped.
The old `run_turn` entry point remains available.

An optional embedding `with_tool_admission` port receives the full unelided pending
request and its fitted failure base, all calls/denials, exact capabilities and held
pure estimator before any goal or registered executor. Actual retained durable events
also reach the host independently of prospective slots and provider fitting. Host-owned reserve/admit/live
dispatch/raw-and-normalized-result/ordered-commit callbacks retain logical capacity
across every chunk. Adapter/model/effort changes queue until the last step hold drops.
Unreserved teardown retires unused local handles; physical workers remain host-owned.
This unpublished [local contract](../.specs/changes/2026-10-03-tool_batch_admission.md)
provides no budget policy, skill/plugin discovery or desktop authority by itself.

## The toolset

Seven _registered_ tools, and the count is the design: each is a mechanism a shell
cannot provide as well, not a convenience wrapper. See
[design decisions](design.md#seven-tools-and-the-count-is-the-design).

| Tool         | What it does                                                                                                                                                                                                                                                                                                  |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `read`       | Reads a file through a 1-based `offset`/`limit` line window, with line numbers, a byte ceiling, and a note on how to continue.                                                                                                                                                                                |
| `write`      | Creates or replaces a file.                                                                                                                                                                                                                                                                                   |
| `edit`       | Replaces text, requiring `old_string` to occur exactly once unless `replace_all` is set — an ambiguous or absent match is refused rather than guessed.                                                                                                                                                        |
| `read_image` | Reads a bounded PNG/JPEG/WebP/GIF (still GIFs only) as typed image content when the model has verified image input; otherwise refuses before file I/O.                                                                                                                                                        |
| `glob`       | Finds files by path pattern, anchored to the workspace root (`*.rs` for the top level, `**/*.rs` at any depth), with a result cap — and a notice naming the cap when matches were dropped, rather than whenever the cap was reached.                                                                          |
| `grep`       | Finds text inside files, grouped by file, optionally narrowed by one `include` glob, with capped matches and truncated lines that say so.                                                                                                                                                                     |
| `bash`       | Runs a program in the workspace root unless a `workdir` says otherwise, with an optional timeout, reporting stdout, stderr, and the exit code. A non-zero exit is a result, not a failure; output is capped and truncated with a notice; the whole process group is killed so grandchildren are not orphaned. |

### `read_video` (optional)

Embedding hosts can implement `VideoSource` directly. `Snapshot::from_owned_file`
retains a host-verified fixed-name absolute source file through an opaque thread-safe
owner, checking receipt shape/128 MiB claimed length without filesystem or credential
I/O. The host verifies authority, immutable bytes, digest and identity and owns cleanup.
Physical decoder workers clone `retain_owner()` and keep that lease through process/
reader join, so call cancellation cannot remove a still-owned copy. Stock FsSource's
existing TempDir/copy behavior is preserved. This local unpublished seam and its
[certificate](../.specs/changes/2026-10-03-video_snapshot.review.md) do not establish
Hype integration, image budget acceptance or native Windows execution.

Off unless `read_video = true`. Needs `ffmpeg` and `ffprobe` (on `PATH`, or in `ffmpeg_dir`); the
agent refuses to start without them, or with a build that lacks the WebM/Matroska, MP4/MOV, AVI
and MPEG demuxers or the H.264, HEVC, MPEG-4, MPEG-2, MJPEG, VP8, VP9 and AV1 decoders. Nothing is
installed for you.

`read_video(file_path, mode, start_ms, end_ms, max_frames, question)` samples up to four JPEGs from
the centres of equal portions of a window of at most 60 seconds, each labelled with its actual
source timestamp. `mode` is `auto` (the default), `frames`, or `analyze`:

- **`frames`** returns the stills to the conversation model. It is refused unless that exact model
  has verified image input, and the refusal happens before the file is opened.
- **`analyze`** sends the same stills, once, to a vision model **on the same provider, plan, endpoint
  and credential account** and returns its text. The conversation model never sees pixels.
- **`auto`** is `frames` when the conversation model has verified image input and `analyze` otherwise.

The analysis models are `claude-sonnet-5-5` (Anthropic), `gpt-6-luna` (OpenAI API and
subscription) and `deepseek-flash` (DeepSeek). z.ai has no model with live image evidence, so on it
`analyze` reports that rather than guessing, and `auto` reaches it for the same reason.

Every result begins with a JSON manifest: the source digest, container, codec, interval, each
frame's timestamp, size and digest, the sampler and decoder versions, warnings, and for an analysis
the provider, plan, model, endpoint origin and reported usage. Audio is omitted and said to be. An
`end_ms` past the end of the video is clamped with a warning; a `start_ms` past it is refused.

Two guards sit outside the tool. **Admission:** a tool may declare how many images a result can carry
and how large each is (`ToolDefinition::with_result_images`; `read_image` declares one, `read_video`
four of at most 512 KiB). Before a step's calls run, the runner reserves that worst case in call
order against the images the newest turn already holds and the model's caps (eight images, 4 MiB
encoded); a call that cannot be admitted is answered with a short failure and never starts, and a
result that exceeds its declaration is replaced by a failure. A tool that declares nothing is neither
admitted against nor bound. **Budget:** each analysis reserves its estimated input plus the whole
output ceiling against `video_analysis_budget` before it is sent, and settles to the usage the
provider reports; a request that fails, reports no usage or is cancelled keeps its whole reservation,
and a budget that cannot cover the call's worst case — its question, the requested frames at 1024
pixels a side, and one answer — refuses it before the file is opened.

Bounds: a 128 MiB source of at most an hour, 8192 pixels an edge; four JPEGs of at most 512 KiB and
1024 pixels on the long edge; 30 seconds per decoder process and 180 for the whole call, whose failure names the stage it
interrupted; 32 KiB of
result text; an analysis is one request with no retry. The tool declares `Execute` access, so the
approval policy applies to it as to `bash`. See [SAFETY.md](../SAFETY.md#read_video) and the
[evidence](vision-evidence.md#read_video).

Only a tool's `name`, `description`, and `parameters` may reach the model; the
executable half is not serialisable, so the allowlist is carried by the types.

### The goal tools

Five more, offered with the seven and dispatched by the loop rather than the registry:

| Tool           | What it does                                                                                                                                                                         |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `get_goal`     | Reads the session's durable objective — its text, whether it is active, paused, complete, or abandoned, and any note on it.                                                          |
| `create_goal`  | Creates a goal for work that will not fit in one turn, from an objective stated as an outcome. Refused while an open goal exists, so a second create cannot silently drop the first. |
| `update_goal`  | Gives the goal a new objective, resumes it (`status: "active"`), or marks it complete (`status: "complete"`) — the last requiring `evidence` of what was checked.                    |
| `pause_goal`   | Suspends a goal that cannot be worked on now, with an optional reason.                                                                                                               |
| `abandon_goal` | Gives up on a goal that cannot be achieved, with a required reason. Terminal, and not completion: the objective was not met.                                                         |

They are the one set that is **not registered**, because their effect is a `goal/change`
record appended to the session log: a tool executor is `'static` and cannot borrow the
session a turn holds, so the agent loop runs them itself — see
[the goal](roadmap.md#shipped-the-goal). They are not a convenience wrapper
either: no shell command can mutate a session's durable objective. The distinction the
seven-tool rule defends is _irreplaceable mechanism versus convenience_, and a goal tool
is on the mechanism side.

Clearing a goal — removing the objective outright — is deliberately **not** among them.
It is the person's decision, and it lives in the interface's `/goal clear`.

They are also **not put to the approval gate**, under any sandbox or approval state. The gate
guards what a tool can do to the workspace and the machine, and a goal call does neither: it
changes a record in the session the agent already holds. A person who wants to overrule the
model's goal does it with `/goal`, and every change the model makes is drawn as a notice. A
tool registered through the published registry under one of these five names is not offered
— the loop would run the goal tool for a call by that name anyway — and a warning names it.
See [the toolset](../crates/nanus-bundle/src/tools/mod.rs).

Image results preserve ordered original text/pixels beside a display summary. PNG/JPEG
files must decode completely, fit 512 KiB and the selected profile's dimensions. Results
allow 32 blocks/four images; a request allows eight images/4 MiB. No resizing or URL fetching
occurs. Anthropic results nest blocks under their call ID/error flag. Chat Completions sends
all sibling tool messages before labelled user attachments, retaining block order and `detail=high`.
Those attachment messages are derived and never added to the session log.

`with_request_budget(output, separate_reasoning)` opts into assembled request preflight
and is required for images. Reasoning already included in the endpoint's output uses zero
separately. The actual provider encoding counts schemas, text, framing and labels, and
validated dimensions determine visual charges. Input must fit the input ceiling; input
plus reservations must fit the smaller caller/model context. Old complete turns are elided
together; a newest turn that cannot fit fails before HTTP. Stock text requests keep their
existing fitting behavior unless a host opts in. See [vision evidence](vision-evidence.md).

Opus/Sonnet 5.5 expose 1M context/128K output metadata, adaptive thinking and signed ordered
assistant replay. Changing system/tools/history strips old signed thinking from derived
requests. The neutral text/tool response must exactly agree with replay blocks.

### What a request costs before the conversation

The system prompt and all twelve schemas — the seven registered tools and the five goal
tools — are sent on **every** request, once per step of every turn, so there is a fixed
floor before the first human word. Measured from the
shipped defaults (`deepseek-flash`, `per_call`, `read_only`, 512 steps/turn) by
serialising the schemas through the DeepSeek encoder:

| Piece                                              | Characters | Estimated tokens |
| -------------------------------------------------- | ---------- | ---------------- |
| `DEFAULT_SYSTEM_PROMPT`                            | 433        | ~108             |
| `## Runtime` section (cwd, model, policy, sandbox) | 133        | ~33              |
| Step-budget sentence                               | 274        | ~69              |
| **System message, as the harness sizes it**        | **844**    | **215**          |
| The seven registered tool schemas, on the wire     | 4,755      | ~1,188           |
| The five goal tool schemas, on the wire            | 2,418      | ~605             |
| **Total, every request**                           | **~8,017** | **~2,008**       |

| Tool           | Estimated tokens |
| -------------- | ---------------- |
| `edit`         | ~207             |
| `grep`         | ~206             |
| `glob`         | ~185             |
| `write`        | ~183             |
| `bash`         | ~166             |
| `read`         | ~151             |
| `read_image`   | ~90              |
| `update_goal`  | ~179             |
| `abandon_goal` | ~121             |
| `create_goal`  | ~117             |
| `pause_goal`   | ~106             |
| `get_goal`     | ~81              |

"Estimated" is the same approximation `context_budget` uses — characters over four, plus
four tokens per message. Each row is rounded on its own, so they need not sum exactly: the
harness sizes the assembled system message at 844 characters and charges 215 estimated
tokens for it, the message framing included. A real tokenizer counts _more_ on JSON, which
is punctuation-heavy, so the provider's reported prompt tokens are the number to check a
budget against. The `cwd` in the runtime section is the one piece that moves with where
you run; the system prompt can be overridden with `system_prompt`, and the runtime section
and budget sentence are appended to whatever replaces it.

Two things follow, and both are recorded rather than hidden. The tool schemas are **not**
charged to `context_budget`: the estimator folds messages, and the schemas ride in the
request outside it, so the real prompt is larger than the budget accounts for. And the
seven-tool limit is a cost decision as well as a design one — an eighth _registered_ tool
is roughly 100–200 more estimated tokens in every request of every turn, which is part of
why the goal tools are five and not fifteen.

## Model providers

Four providers, selected with `provider` in the configuration. Each is an adapter
that implements `LlmPort`, and nothing in the tools, the domain, or the loop knows
which one is in use.

| Provider             | Adapter                   | Models offered                                                                                                              | Plans                  |
| -------------------- | ------------------------- | --------------------------------------------------------------------------------------------------------------------------- | ---------------------- |
| `deepseek` (default) | `nanus-adapter-deepseek`  | `deepseek-flash`, `deepseek-v4-pro`                                                                                         | `api`                  |
| `zai`                | `nanus-adapter-openai`    | `glm-5.3-flashx`, `glm-5.3-flash`, `glm-5.3`, `glm-5.2`                                                                     | `api`, `coding`        |
| `anthropic`          | `nanus-adapter-anthropic` | `claude-sonnet-5-5`, `claude-opus-5-5`, `claude-fable-5-1`, `claude-haiku-4-5-20251001`, `claude-sonnet-5`, `claude-opus-5` | `api`                  |
| `openai`             | `nanus-adapter-openai`    | `gpt-6-astra`, `gpt-6.1-sol`, `gpt-6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`                                  | `api`, `subscription`¹ |

¹ The OpenAI `subscription` plan is a ChatGPT account **authorized with OAuth** rather than a typed
key: choosing it runs the device flow, files the token set under `openai:subscription`, and reaches
the account through the **Responses API** — the items-shaped wire that backend speaks, with the
grant's access token as the bearer, the account named in its own header, and an expired access token
renewed from the refresh token rather than sent and refused.
On the `api` plan, models from `gpt-5.6` on (read from the id's version, so a later model is
covered) go to the Responses API too, chosen per request; older ids stay on chat completions. A
conversation holding images stays on chat completions, because the Responses encoder has no image
items yet. The plan starts on `gpt-6.1-sol` at `high` effort; a `reasoning_effort` in the configuration, or a
remembered selection, wins over that default.

- **A plan is an endpoint, a default model, a wire, and a credential.** z.ai's `coding` plan is
  the same protocol and a key of its own at a different host; OpenAI's `subscription` plan is a
  `ChatGPT` account, authorized in a browser, that serves the Responses API rather than chat
  completions. `base_url` moves where a request goes, not which of the two it is.
- **OpenAI's `coding` plan is gone.** It was a default model — `gpt-5.3-codex` — on the API with
  the API key (OpenAI has since shut that model down on the API, and it is no longer offered), so
  `plan = "coding"` with `provider = "openai"` is refused by name rather than kept as a second
  spelling of the API plan. z.ai's `coding` plan is unaffected: it is a different host and a
  different key.
- **Streaming responses** over SSE, with reasoning content and tool calls
  reassembled from their frames. DeepSeek and z.ai send `data:`-framed chunks ending
  in `[DONE]`; Anthropic sends event-typed frames ending in `message_stop`.
- **Exact wire selection for library hosts.** `OpenAiConfig::set_protocol_preference` accepts
  `Automatic` (the stock default) or `Exact(ChatCompletions | Responses)`. Exact selection overrides
  model routing and the fallback set by `set_protocol`; an incompatible z.ai/subscription wire is
  refused without changing the preference. Endpoint edits are rechecked before construction and
  dispatch. Capability/image evidence is specific to the selected Responses wire and its exact
  known API/subscription endpoints; Chat, custom gateways and unprofiled models remain Unknown.
  Exact Responses output controls follow the actual endpoint. A requested explicit ceiling is
  refused when that endpoint cannot honor it. Automatic retains legacy plan handling and routing;
  raw `encode` remains an unchecked wire-inspection helper, not a capability or acceptance claim.
- **Optional response budgets for library hosts.** Construct `nanus_ports::ResponseLimits`
  and install it with each API config's `set_response_limits` before constructing the adapter.
  Budgets cover partial SSE lines, JSON data payloads/assembled call content, raw response bytes,
  data-payload count, tool/block slots and non-success HTTP body bytes. Checks precede owned
  buffer extensions and JSON decoding; completed signed Anthropic replay also checks its full
  serialized envelope. These are logical byte/count bounds, not allocator/TLS/client RSS bounds.
  Bounded streams reject invalid UTF-8, malformed JSON and incomplete protocol termination,
  discard retained partial calls/replay on failure and release the HTTP body at failure/termination.
  The count is SSE data payloads, including unknown kinds, rather than the number of `LlmEvent`s
  produced from them; hosts may additionally bound their own decoded events and stored records.
  Limits are absent by default: stock CLI/TUI composition retains legacy framing and EOF handling.
  Hosts still own request/turn deadlines, cancellation and teardown of tool effects.
- **Exact DeepSeek text limits.** On `https://api.deepseek.com`, the exact ids
  `deepseek-flash` and `deepseek-v4-pro` each report 1,048,576 combined-context tokens and
  393,216 maximum output tokens. The input bound is that combined-context upper bound,
  with every actual output/separate-reasoning reservation still deducted from the context.
  The [models response](https://api-docs.deepseek.com/api/list-models/) supplies each entry
  independently. Custom endpoints and ids stay unknown. Zero or oversized output refuses
  before HTTP even without an explicit context budget; a valid request override takes
  precedence over the configured output default. Text metadata does not qualify images:
  Flash remains Unknown, and Pro is Unsupported because its contract lists text input only.
- **Local tool-support admission metadata.** `LlmPort::tool_call_support(model, request_effort)`
  returns `ToolCallSupport::{Supported, Unsupported, Unknown}` for the actual endpoint/wire/model
  and effective effort; absence inherits the adapter default, while explicit `ReasoningEffort::None`
  disables reasoning where representable. Legacy ports default Unknown. Official exact DeepSeek
  Flash/Pro and Anthropic Opus/Sonnet 5.5/Fable 5.1 ordinary tools have local entries. Six exact
  OpenAI models have Responses entries; Astra/Sol Chat refuses tools, Luna Chat requires effective
  None, and other Chat/subscription/custom combinations remain Unknown. Known z.ai API entries
  are scoped separately below; Coding Plan has no inherited API evidence. Images and token
  metadata remain independent. OpenAI/Anthropic known Unsupported tool definitions or call/result
  replay refuse before HTTP even when the current tool list is empty; unknown stock behavior and
  valid text-only mapping remain. Strict hosts must query/delegate and require Supported themselves.
  Anthropic None/Minimal tool controls refuse rather than qualifying through coercion to low;
  native low effort is supported. [Contracts and scoped verification](../.specs/changes/2026-09-30-library_embedding_and_multimodal_results.md#local-generic-tool-support-implementation--2026-10-03).
- **Exact z.ai API admission.** Official Chat requests for `glm-5.3-flashx`, `glm-5.3-flash`,
  `glm-5.3` and `glm-5.2` have local text/output and ordinary function-tool metadata. The
  advertised 1M context is conservatively interpreted as decimal 1,000,000; the exact output
  maximum is 131,072. GLM-5.3 variants accept low/high/max on the API; GLM-5.2 accepts all seven
  neutral spellings with enabled thinking. Known API models start at max. Explicit choices win
  and invalid choices refuse before HTTP. Direct text requests also check context/output and
  the 128-function limit without requiring a caller budget. Coding Plan, custom endpoints and
  unknown ids retain stock defaults/behavior and Unknown metadata. Text-only GLM-5.3/5.2 image
  input is Unsupported; Flash/FlashX images remain Unknown with no image profile or promotion.
  [Contract, fixtures and limits](../.specs/changes/2026-10-03-zai_api_admission.review.md).
- **Tool calling** for all four, including DeepSeek's reasoning passback and its
  empty assistant turn sent as `content: ""`, and Anthropic's `tool_use` /
  `tool_result` content blocks.
- **Usage accounting** — prompt, cached, and generated tokens, including how much of
  the generation was thinking, decoded from each provider's own spelling.
- **Request controls**: `max_tokens` (default 128000, capped at the provider's
  documented ceiling because a request above it is refused rather than truncated)
  and `reasoning_effort` (`minimal` / `low` / `medium` / `high`; when the file names none, the
  plan's own default applies — `high` for OpenAI's `subscription` plan, `max` for known z.ai API
  models on their verified endpoint — and then `medium`).
  The configured step reaches DeepSeek, z.ai (as a thinking mode) and OpenAI (as its own
  scale). Anthropic is different: the file's `reasoning_effort` is **not** applied to it, and a
  step the reader _chooses_ — in the interface, or remembered from a switch — is sent as
  `output_config.effort` (`low` through `max`) to the 5-series models (Sonnet 5.5, Opus 5.5, Fable 5.1,
  and the earlier Sonnet 5 and Opus 5). Haiku 4.5 takes no effort at all, so none is sent and none is
  offered. Opus 5.5, Sonnet 5.5 and Fable 5.1 also request adaptive thinking.
- **Retired model ids do not resolve**, and a model id belongs to the provider that
  offers it: naming a DeepSeek id with `provider = "openai"` is a request the
  provider refuses rather than a quiet substitution.
- **An agent whose provider has no credential still starts.** The composition
  substitutes a placeholder adapter that names the model that would answer and reports
  the missing credential on the first request, so the interface opens and the reader can
  configure a provider with `/provider` rather than being refused the program. Every
  other failure to compose still refuses. See
  [switching providers](tui.md#switching-providers).
- **`nanus config` reports the resolution**, so which provider, plan, model, and
  endpoint a run will use is answerable without running it.
- **The provider seam is the boundary.** `LlmPort` is the port, `nanus-bundle`
  owns the table of providers, and adding one is an adapter crate plus a row —
  nothing in the tools or the domain changes.

### What the next start begins from

The interface can change the provider, the model, and the effort of a running agent
(`/provider`, `/model`, `/effort`, `Alt+P`, `Alt+T`). The moment it does, the agent writes the
whole selection to `<nanus home>/selection.toml`, and the next start — a run, a service, or an
interface — begins from it rather than from the file's provider, model, and effort. So choosing a
model in the interface and quitting is how the default is changed.

The record is written on a **change** and never on a start: merely opening the interface does not
pin the configuration, so a file edited afterwards is still what an untouched run uses. Deleting
`selection.toml` makes the configuration the whole answer again, and `nanus config` reports the
selection a run would actually use, record included.

Two details are deliberate. The record stores the plan with the provider and a model with either,
because a provider without them would send the previous provider's model to the new host. And it
stores the effort in the port's own vocabulary rather than the configuration's four-value field,
because a model's scale has steps — `none`, `max` — that the field cannot name.

## Credentials

A provider key is a secret, so it is not in the configuration file and not a
field of any type that reaches a log.

- **`nanus auth set <provider>`** reads a key from standard input and stores it, and
  **`nanus auth login <provider>`** runs the device flow a plan reached with OAuth needs: the page
  and the code are printed, and the token set is filed when the service confirms.
  `nanus auth clear <provider>` removes either, and `nanus auth status` reports which
  providers have one. The value is never printed, and never taken as an argument,
  so it does not appear in a process list.
- **A chain of stores**, tried in order: the macOS keychain, a `0600` file under
  `<nanus home>/secrets/`, then the provider's environment variable
  (`DEEPSEEK_API_KEY`, `ZAI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`). The
  first store holding a value answers, and a store that cannot answer — a locked
  keychain on a detached service — does not hide a value another store holds.
- **The account names what the key is for**: the provider's name, or the provider and plan joined
  for a plan with a key of its own (`zai:coding`), so a key stored for one provider or plan can
  never be sent to another.
- **The stores are a port.** `SecretPort` is the boundary and each store is a
  `SecretBackend`, so another platform store is an implementation rather than a
  change to the harness. Only the macOS keychain and the file and environment
  fallbacks ship today. See [SAFETY.md](../SAFETY.md#secrets).

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
  moved. A name is a single word — no namespaces — and case does not make a second
  one: `Nightly` and `nightly` are the same name, resolved either way, with the
  spelling a session was named with kept for display.
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
ignored, and there is no field that can hold a credential. See
[configuration](../crates/nanus-adapter-config).

Every provider field is optional, and absent means "the provider's own answer
applies": a file that names none of them runs `deepseek` with its own host and
model, and a file that sets `provider` alone gets that provider's host and model.
An unknown provider or plan is refused at startup with a sentence naming the ones
this build offers.

| Field                   | Default                           | Values                                                                                                                    |
| ----------------------- | --------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `provider`              | `deepseek`                        | `deepseek`, `zai`, `anthropic`, `openai`                                                                                  |
| `plan`                  | the provider's default            | `api`, `coding` (z.ai), `subscription` (OpenAI, authorized with OAuth)                                                    |
| `base_url`              | the plan's endpoint               | an override, for a proxy or a gateway                                                                                     |
| `model`                 | the plan's or provider's default  | any id the provider serves                                                                                                |
| `max_tokens`            | `128000`                          | per-response budget, capped at the provider's ceiling                                                                     |
| `reasoning_effort`      | the plan's default, then `medium` | `minimal`, `low`, `medium`, `high`                                                                                        |
| `approval_policy`       | `per_call`                        | `per_call`, `permitted`, `all_calls`                                                                                      |
| `sandbox_mode`          | `read_only`                       | `read_only`, `workspace_write`, `danger_full_access`                                                                      |
| `max_steps_per_turn`    | `512`                             | steps in one turn                                                                                                         |
| `context_budget`        | `64000`                           | estimated prompt tokens for one request                                                                                   |
| `max_parallel_tools`    | `4`                               | how many of a step's calls may be in flight at once                                                                       |
| `tui_detail`            | `compact`                         | `compact`, `full`                                                                                                         |
| `markdown`              | `true`                            | render the model's answers as markdown                                                                                    |
| `mermaid`               | `true`                            | draw `mermaid` fences as text diagrams                                                                                    |
| `read_video`            | `false`                           | offer the optional [`read_video`](#read_video-optional) tool                                                              |
| `ffmpeg_dir`            | `PATH`                            | the directory holding `ffmpeg` and `ffprobe`                                                                              |
| `video_analysis_budget` | `200000`                          | tokens `read_video` may spend on analysis requests per agent process                                                      |
| `system_prompt`         | built-in                          | override the system prompt                                                                                                |
| `workspace_root`        | the current directory             | root the tools are confined to                                                                                            |
| `service_socket`        | `<nanus home>/run/agent.sock`     | where a service listens                                                                                                   |
| `service_log`           | `<nanus home>/nanus-service.log`  | where a detached service writes                                                                                           |
| `config_version`        | the build's version               | the schema the file was written with; a newer one is refused, and a missing one is read as the pre-1.0 shape and migrated |

Environment variables:

| Variable                           | Effect                                                                                                  |
| ---------------------------------- | ------------------------------------------------------------------------------------------------------- |
| `DEEPSEEK_API_KEY`                 | DeepSeek credential. The last store in the chain; never stored, serialised, or rendered by the harness. |
| `ZAI_API_KEY`                      | z.ai credential, for the `api` plan. The `coding` plan has its own                                      |
| (`ZAI_CODING_API_KEY`).            |
| `ANTHROPIC_API_KEY`                | Anthropic credential.                                                                                   |
| `OPENAI_API_KEY`                   | OpenAI credential, for the `api` plan. The `subscription` plan is authorized,                           |
| not keyed: see `nanus auth login`. |
| `NANUS_CONFIG`                     | Override the configuration file path.                                                                   |
| `NANUS_HOME`                       | Override the session-store home (and the default socket and log paths).                                 |
| `NANUS_TUI`                        | Override the path to the interface binary.                                                              |
| `NO_COLOR`                         | Render with no colour at all, keeping bold and italic.                                                  |
| `RUST_LOG`                         | Tracing filter for the service and core logs.                                                           |

## The command line

Global options on `nanus`: `--verbose`, `--quiet`, `--config <PATH>`. `--quiet` is
accepted and does nothing, because the default already is what it asks for — stdout
carries the answer and nothing else on every run — and it conflicts with `--verbose`,
which asks for progress on stderr.

| Command                                                           | What it does                                                                                                 |
| ----------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| `nanus run [--name NAME \| --resume NAME\|ID] <TASK>`             | One prompt, one answer on stdout, then exit.                                                                 |
| `nanus tui` / `nanus ui`                                          | Start the interface against a shell-scoped agent.                                                            |
| `nanus tui --connect [--socket PATH]`                             | Talk to a `nanus service` instead.                                                                           |
| `nanus tui --resume REF` / `--name NAME`                          | Open or record a particular session.                                                                         |
| `nanus tui --session [ID] [--scroll ROWS]`                        | Read a recorded transcript; no key needed.                                                                   |
| `nanus service start [--foreground] [--socket PATH] [--log PATH]` | Start a service, detached by default.                                                                        |
| `nanus service stop [--socket PATH]`                              | Ask a running service to stop.                                                                               |
| `nanus service status [--socket PATH]`                            | Report whether one is running, and which sessions it holds; non-zero when nothing answers.                   |
| `nanus config`                                                    | Print the effective configuration and the provider, plan, model, and endpoint it resolves to; no key needed. |
| `nanus auth set <PROVIDER>[:<PLAN>]`                              | Store a key, read from standard input.                                                                       |
| `nanus auth login <PROVIDER>[:<PLAN>]`                            | Authorize a plan that is reached with a browser rather than a key; waits for the service.                    |
| `nanus auth clear <PROVIDER>[:<PLAN>]`                            | Remove a stored credential, key or authorization.                                                            |
| `nanus auth status`                                               | Report which providers and plans have a credential, and where a key is read from; no key needed.             |
| `nanus sessions`                                                  | List recorded sessions, newest first; no key needed.                                                         |
| `nanus sessions name <NAME> <SESSION>`                            | Record or change a session's name.                                                                           |
| `nanus sessions delete <NAME\|ID>`                                | Remove a session and release its name.                                                                       |
| `nanus sessions show [--json] <NAME\|ID>`                         | Report what a session ran under and what it spent; no key needed.                                            |

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
- **Mouse support**: the wheel scrolls, a click in the composer places the caret,
  and a drag in the transcript selects text.
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
- **Slash commands**: `/exit`, `/quit`, `/stats`, `/help`, `/clear`, `/model`, `/effort`,
  `/provider`, `/goal`, and `/copy`. Anything else is named in the transcript rather than sent to
  the model.
- **A durable goal** (`/goal`): one objective per session, persisted in the session log, with a
  lifecycle a person drives — `/goal <objective>` sets one, a bare `/goal` reads it, and
  `pause`, `resume`, `complete`, `abandon`, and `clear` move it. It survives a resume, it is
  shown to every client attached to the session, and the model can read and move it too; see
  [the goal tools](#the-goal-tools). `/goal` is the one command that reaches the agent rather
  than being answered on screen, because a session belongs to the agent. See
  [the interface](tui.md#commands).
- **The key list on screen** (`?`, or `/help`), scrolled from one table so a
  binding cannot be documented in one place and forgotten in another.
- **Switching provider at runtime** (`/provider`): a chooser over the providers and plans the
  agent offered, or a named one. A provider with no credential is answered with the question of
  whether to store a key, and the key is filed by the agent, which owns the store. A change
  rebuilds only the model adapter, so the conversation survives it.
- **The settings a key changes**: the approval state (`Shift+Tab`, in a dialog that
  says what each state grants), the model (`Alt+P` cycles, `/model` opens a selector,
  `/model <id>` names one), and the reasoning effort (`Alt+T` cycles the current
  model's own steps, `/effort` opens a chooser over them). Each is drawn where a
  reader can see which is in force.
- **`@` file mentions**, completed from the workspace by `Tab` and expanded to the
  path — the model reads the file with a tool rather than being handed its
  contents.
- **Pasting an image** (`Ctrl+V`): the clipboard is read by the platform's own
  tool, the bytes are checked by their magic number, and the file is written inside
  the workspace so `read_image` can reach it.
- **Selecting and copying**: drag with the mouse, anywhere in the interface —
  over a dialogue or the composer as readily as over an answer — or extend the
  transcript with `Shift` and a movement key, then `Ctrl+C`. `/copy` takes the
  newest answer without pointing at it. The clipboard is the platform's own tool,
  falling back to the terminal's `OSC 52`. See
  [the interface](tui.md#selecting-and-copying).
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
- **A client that attaches mid-turn catches up.** The turn in flight crosses as one
  backlog frame — the prompt, the steps, the deltas so far, and any question the turn
  is waiting on — so a transcript read from a joined session begins at the beginning of
  the turn rather than in its middle. A turn that has ended is read from the store as
  before, so nothing travels twice.
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
  a plugin reverts in reverse _activation_ order — not reverse declaration order, which
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
- **Secrets never reach a log or a request body**: a credential comes from the
  secret store, is absent from the configuration type, is wrapped in a type that
  redacts its own `Debug`, and is read out only where a request is built.
- **Five quality gates** — `cargo fmt`, `cargo clippy`, `cargo nextest`, the
  doctests, and the interface's view layer built without its runtime half — with the
  tests that matter most and the bugs verification found in
  [testing and verification](testing.md).

## Not supported yet

The honest list lives in [status](status.md#known-limits); the headline items:

- **Image input is Supported for nine exact models.** Opus 5.5, Sonnet 5.5, and on the `api` plan
  over the Responses API GPT-6 Astra, GPT-6.1 Sol, GPT-6 Luna and GPT-5.6 Sol, Terra and Luna passed
  live follow-ups; see [vision evidence](vision-evidence.md). The same six OpenAI models are Supported on the `ChatGPT`
  subscription backend, which was run separately. `deepseek-flash` is Supported on its own endpoint
  with a measured profile. Other models and z.ai stay Unknown and refuse images; DeepSeek V4 Pro is
  explicitly Unsupported.
- **Only the macOS keychain ships as a platform store.** The port and the backend
  trait are in place, so another is an implementation plus a line in the chain.
- **The link is local.** Unix sockets and Windows SID-named pipes share the same frames.
  Windows service lifecycle and the Job Object shell passed native runtime validation
  (see [status](status.md) and [the transport design and evidence](link-transports.md)).
  There is no remote mode. On Windows both ends prove they are the same user before a frame is
  exchanged, so a pipe name squatted by another account is refused rather than talked to.
- **The sandbox is not OS-enforced.** Nothing confines an approved program's
  writes, its network access, or its process table.
- **A session is claimed for writing.** Two writers on one log cannot silently lose a turn:
  the second is refused with a sentence naming the holder, and attaching to a live session
  attaching to a live session is the supported way to share one. The claim is a lock the
  operating system holds on a file beside the log, so it is exact between `nanus` processes
  and advisory against the rest: a process that writes the log directly is not stopped.
- **No remote fetch from the renderer**: a fenced block is highlighted by a lexer
  inside the view, and an image is a placeholder or a pasted file, never a URL
  fetched on a model's say-so.
- **The interface's `!` command is not recorded and not confined.** It is your
  shell rather than the agent's: see [SAFETY.md](../SAFETY.md).
