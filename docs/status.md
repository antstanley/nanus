# Status

Working, end to end. Progress is honest rather than flattering — the last section is
limits worth knowing before you depend on something.

|                                                     |                                                                                                 |
| --------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| Kernel, domain, ports, all four adapters            | complete                                                                                        |
| Toolset, agent loop, composition                    | complete                                                                                        |
| Core CLI: `run`, `config`, `sessions`               | complete                                                                                        |
| The interface, as its own binary                    | view layer tested headlessly; raw-mode input needs a real terminal                              |
| The local link between them                         | protocol, client, and server; tested over real sockets with a scripted model                    |
| Named, resumable sessions                           | names in the store, `--name` / `--resume` on `run` and `tui`, `nanus sessions name`             |
| Sessions held open by an agent                      | listed by `nanus service status`, attached to by `--resume`, watched by several clients at once |
| `nanus service`                                     | `start` (detached and `--foreground`), `stop`, `status`; detached lifetime verified by hand     |
| Live path (streaming, tool calls, results fed back) | **verified against the real API**                                                               |

Composing a harness is `compose(&config).await` for the adapters, then
`Pending::start()` outside the runtime for the kernel — the two phases exist because
`block_on` cannot be called from inside a runtime, and the split is enforced by types
rather than by remembering.

## The three modes

| Mode                  | Agent lifetime           | Reached by                    |
| --------------------- | ------------------------ | ----------------------------- |
| `nanus run <task>`    | until the turn completes | stdout                        |
| `nanus` / `nanus tui` | the interface's          | `nanus-tui --link <socket>`   |
| `nanus service`       | until stopped            | `<nanus home>/run/agent.sock` |

The agent is the same object in all three. What differs is a lifetime and a transport, and
the transport is the same one in all three — so there is one code path that runs a turn
for an interface to watch, rather than one for "the interface we linked" and another for
"the interface over there".

## Known limits

- **Managed context is opt-in and unmeasured.** Its mechanics are covered by deterministic tests
  against the real runner, store, shell and adapters' wire encoders, but no live provider has yet
  been sent a managed request, and no held-out quality or cost evaluation has been run, so there
  is no claim that it helps; the default stays legacy. See
  [what is and is not verified](context-management.md#what-is-and-is-not-verified).

- **`read_video` is verified on Anthropic, OpenAI and DeepSeek.** z.ai has no model with live image
  evidence and no stored credential, so there `analyze` and `auto` report that instead of guessing.
  DeepSeek's image price is undocumented, so its profile reserves about twice what was measured. FFmpeg decoding was
  exercised on macOS with FFmpeg 9.0.2 only. A dropped call kills its decoder (tested on Unix with a
  stand-in process), but the Linux and Windows decoder paths, and Windows process teardown, are
  untested; there is no grandchild test because FFmpeg starts none. Admission is by declared worst
  case, so two four-frame reads in one step are refused by the 4 MiB request cap (the second is
  asked to retry), which is conservative by design. The analysis budget is per agent process and
  does not survive a restart. See [the evidence](vision-evidence.md#read_video).
- **Live tool calling is replayed from a captured trace.** `live_wire.rs` replays a real
  response recorded from `api.deepseek.com` — a `bash` call whose arguments arrived a few
  characters at a time — byte for byte, in `crates/nanus-bundle/tests/data/`. That is what
  makes the framing a tested claim rather than an assumption; what it cannot catch is a
  shape the API starts sending _tomorrow_, and if one appears,
  [`wire.rs`](../crates/nanus-adapter-deepseek/src/wire.rs) is the only adapter file
  involved.
- **The TUI has no automated end-to-end test.** Raw mode needs a real terminal, so the
  alternate screen and the drawing itself are exercised by hand. What _is_ covered
  automatically are the parts that can be: key handling, scrolling and wrapping against
  ratatui's `TestBackend`; frames from the link landing in the right entries; the refusal
  of a missing terminal, since `ratatui::init` panics rather than returning when there is
  no terminal to take; and the link itself, over real sockets in a temporary directory
  with a scripted model (`crates/nanus-link/tests/link.rs`). Submitting a prompt was broken
  from the first commit until it was first typed into — see
  [the bugs verification found](testing.md#the-bugs-verification-found).
- **The link is local and trusts its peer.** Unix uses a socket in the user's nanus home,
  `0600` inside a `0700` directory. Windows SID-named pipes, service detaching, and the Job Object
  shell passed native Windows runtime validation alongside Linux and macOS in
  [the transport matrix](https://github.com/antstanley/nanus/actions/runs/36852736607).
  There is no remote mode. Windows uses the default descriptor and proves the same user at
  both ends with a key-based handshake, so a pipe squatted by another account is refused; see
  [the service page](service.md#known-limits) and [the transport design](link-transports.md).
- **The context budget is an estimate, and the policy is drop-oldest.** A prompt is bounded
  by `context_budget` in estimated tokens — characters over four, plus a small cost per
  message — because there is no tokenizer in the harness and the provider's real count
  arrives only with the response. It is not a summarising policy: the oldest _turns_ are
  dropped with a notice the model reads, and nothing rewrites what they said. A single turn
  larger than the whole budget is refused rather than sent or silently shortened. The
  estimate also covers **messages only**: the system prompt is a message and is counted,
  but the seven tool schemas are sent on every request outside that accounting, so the real
  prompt carries roughly 1,200 estimated tokens more than `context_budget` knows about. See
  [what a request costs](features.md#what-a-request-costs-before-the-conversation).
- **A session is claimed, not locked.** A writer claims the session it holds — a `nanus run`
  for its run, an agent for as long as it holds the session — and a second writer is refused
  with a sentence naming the holder, so `nanus run --resume x` against a service serving `x`
  says what to do instead of overwriting it. The claim is a file beside the log that the
  operating system locks while a writer holds it, so it is exact between `nanus` processes and
  advisory against everything else: a process that writes the log directly is not stopped, a
  holder that exits releases the lock by exiting, and `nanus sessions delete` does not consult
  it. Attaching to
  a live session is still the supported way to share one, and it is what the interface does.
- **A session is held in memory while an agent holds it.** Bounded at 32 idle sessions,
  least-recently-used first, and never at the cost of a running turn or an attached
  client. A session that is let go is still on disk and reloads on the next attach.
- **A client that attaches mid-turn is caught up with that turn, and no further.** The
  turn in flight crosses as one `Backlog` frame — the prompt, the steps, the deltas so far,
  and any question the turn is waiting on — and the live turn continues from there. The
  frames _inside_ the batch are folded (adjacent deltas joined), so a client that was
  attached all along and one that arrived late hold the same text but not necessarily the
  same number of frames.
- **One agent, one thread.** A service serves several clients and their turns interleave
  cooperatively, because the kernel is single-threaded and its futures are not `Send`.
  Concurrency is not parallelism, and there is no worker pool. A step's tool calls do run
  together — cooperatively, capped by `max_parallel_tools`, and interleaved at their await
  points — but a call that blocks the thread blocks all of them.
- **Four providers, every plan they ship usable.** `deepseek`, `zai` (its API and a coding plan
  with a key of its own), `anthropic`, and `openai` (its API and a `ChatGPT` subscription
  authorized over OAuth, which speaks the Responses API through an encoder of its own) all run;
  on the API plan, `gpt-5.6` and later models are sent to the Responses API too, chosen per request.
  The subscription backend refuses an output ceiling, so none is sent there, and its plan starts on
  `gpt-6.1-sol` at `high`. Opus/Sonnet 5.5 and Fable 5.1 request adaptive thinking; Opus/Sonnet 5.5
  preserve signed blocks for unchanged-prefix replay.
- **Embedding is a separate build boundary.** The minimal runner uses caller-owned local
  adapters. Its native Windows/macOS matrix passed both minimally and with explicit
  provider dependencies; [tested revision and run](vision-evidence.md). Stock shell/link/service
  passed the separate native Windows/Linux/macOS transport matrix; [evidence](link-transports.md).
- **Exact protocol preference is available to hosts.** Automatic remains the stock model/plan
  route. Exact selects one wire or refuses before HTTP; capability queries no longer inherit
  Responses image evidence on Chat or an arbitrary proxy. The nine new fixtures establish local
  routing/refusal behavior; they do not promote a new protocol or prove a downstream host
  selected the API.
- **Pre-event response limits are opt-in library policy.** All three API adapters accept the
  same caller-selected budgets, including OpenAI's Chat and Responses decoders. Local HTTP
  fixtures cover termination, oversized bodies and cancellation without a live provider call.
  Stock composition selects no response limits. Native Windows/live-provider evidence for this
  new policy remains separate; it does not establish an embedding consumer's selected settings.
- **Generic tool support has exact local entries.** The defaulted query is independent of image
  and text-limit metadata. Fourteen new fixtures cover actual replay encoding, literal effort,
  endpoint isolation and refusal before local TCP contact; 262 affected-crate tests pass. Exact
  Responses/Chat distinctions are preserved without automatic protocol or effort changes. A strict
  embedding wrapper must delegate the query and require Supported, including before replay.
  Stock Unknown behavior is preserved. Full stock-suite credential isolation, live/native-platform
  acceptance and consumer immutable adoption remain separate gates; [scope](../.specs/changes/2026-09-30-library_embedding_and_multimodal_results.md#local-generic-tool-support-implementation--2026-10-03).
- **Complete-batch admission is an optional local embedding seam.** Full and fitted
  request/durable-event projections plus owned callbacks cover every goal/registered tool, raw and
  normalized results, and held model selection. Final credential-free tests/lint pass;
  the [unpublished scope](../.specs/changes/2026-10-03-tool_batch_admission.md) grants no
  source/worker/payment authority or consumer budget implementation. Downstream adoption,
  credential-aware stock tests and native Windows execution remain separate.
- **Vision is promoted for nine exact models.** Opus 5.5, Sonnet 5.5, GPT-6 Astra, GPT-6.1 Sol,
  GPT-6 Luna and GPT-5.6 Sol, Terra and Luna passed live image and call-reference follow-ups; other models refuse image HTTP
  (DeepSeek V4 Pro is explicitly Unsupported; the rest stay Unknown). `deepseek-flash` is the ninth.
  [Evidence and limits](vision-evidence.md).
- **Only the macOS keychain ships as a platform secret store.** `SecretPort` and the
  `SecretBackend` trait are the seam, and a `0600` file and the environment are the
  fallbacks that work everywhere, but a Linux Secret Service or a Windows Credential
  Manager store is an implementation that does not exist yet.
- **A plan is an endpoint, a wire, and a credential.** z.ai's coding plan is the same protocol
  and a different key at a different host; `OpenAI`'s subscription is a `ChatGPT` account,
  authorized in a browser, whose grant is a token set the agent renews rather than a key it sends.
  The wire a request takes is read from the plan and not from how the credential was obtained, so
  pointing a `base_url` somewhere else moves the request without changing what it is.
- **The sandbox is reported, not OS-enforced.** `nanus-adapter-local` checks the working
  directory and returns the policy, but installs no OS-level confinement. See
  [SAFETY.md](../SAFETY.md).

## Where the research lives

The dependency research, the process-execution measurements, the raw crates.io responses
and the probe workspaces that produced them live in a separate repository —
**`antstanley/nanus-research`** (private). They are evidence rather than shipped code, and
carrying them here meant a clone of the product also fetched research scratch.

One report stays in this repository, because it documents the framework the kernel
_implements_ rather than the research that led to it:
[`cordis-mechanisms-report.md`](../cordis-mechanisms-report.md).

## How composition is staged

`compose(&config).await` builds the adapters; `Pending::start()` mounts the kernel. The
two phases exist because `block_on` cannot be called from inside a runtime, and the
split is enforced by types — `Pending` is the seam that makes the ordering a compile-time
fact rather than a convention someone has to remember. Getting it wrong produced
"cannot start a runtime from within a runtime" on every command, which is how it was
found.

The same rule shapes the interface and the service, with one addition: a turn that an
interface is watching is a `!Send` local task, so it needs `block_on_local`, which enters
a `LocalSet` _and_ runs the runtime. Entering a local set without running it leaves every
spawned task un-polled, which looks exactly like a hung agent.
