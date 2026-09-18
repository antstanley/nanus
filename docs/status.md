# Status

Working, end to end. Progress is honest rather than flattering — the last section is
limits worth knowing before you depend on something.

| | |
|---|---|
| Kernel, domain, ports, all four adapters | complete |
| Toolset, agent loop, composition | complete |
| Core CLI: `run`, `config`, `sessions` | complete |
| The interface, as its own binary | view layer tested headlessly; raw-mode input needs a real terminal |
| The local link between them | protocol, client, and server; tested over real sockets with a scripted model |
| Named, resumable sessions | names in the store, `--name` / `--resume` on `run` and `tui`, `nanus sessions name` |
| Sessions held open by an agent | listed by `nanus service status`, attached to by `--resume`, watched by several clients at once |
| `nanus service` | `start` (detached and `--foreground`), `stop`, `status`; detached lifetime verified by hand |
| Live path (streaming, tool calls, results fed back) | **verified against the real API** |

Composing a harness is `compose(&config).await` for the adapters, then
`Pending::start()` outside the runtime for the kernel — the two phases exist because
`block_on` cannot be called from inside a runtime, and the split is enforced by types
rather than by remembering.

## The three modes

| Mode | Agent lifetime | Reached by |
|---|---|---|
| `nanus run <task>` | until the turn completes | stdout |
| `nanus` / `nanus tui` | the interface's | `nanus-tui --link <socket>` |
| `nanus service` | until stopped | `<nanus home>/run/agent.sock` |

The agent is the same object in all three. What differs is a lifetime and a transport, and
the transport is the same one in all three — so there is one code path that runs a turn
for an interface to watch, rather than one for "the interface we linked" and another for
"the interface over there".

## Known limits

- **Live tool calling is replayed from a captured trace.** `live_wire.rs` replays a real
  response recorded from `api.deepseek.com` — a `bash` call whose arguments arrived a few
  characters at a time — byte for byte, in `crates/nanus-bundle/tests/data/`. That is what
  makes the framing a tested claim rather than an assumption; what it cannot catch is a
  shape the API starts sending *tomorrow*, and if one appears,
  [`wire.rs`](../crates/nanus-adapter-deepseek/src/wire.rs) is the only adapter file
  involved.
- **The TUI has no automated end-to-end test.** Raw mode needs a real terminal, so the
  alternate screen and the drawing itself are exercised by hand. What *is* covered
  automatically are the parts that can be: key handling, scrolling and wrapping against
  ratatui's `TestBackend`; frames from the link landing in the right entries; the refusal
  of a missing terminal, since `ratatui::init` panics rather than returning when there is
  no terminal to take; and the link itself, over real sockets in a temporary directory
  with a scripted model (`crates/nanus-link/tests/link.rs`). Submitting a prompt was broken
  from the first commit until it was first typed into — see
  [the bugs verification found](testing.md#the-bugs-verification-found).
- **The link is Unix-only and trusts its peer.** A Unix domain socket in the user's own
  nanus home, `0600` inside a `0700` directory. No remote mode, no Windows (the workspace
  already depends on `nix` for process groups), and no defence against a process already
  running as the same user — such a process can read the workspace and the session log
  regardless. See [the service page](service.md#known-limits).
- **A session is not locked.** `nanus run --resume x` and a service holding `x` are two
  writers on one log, and the second save wins. Attaching to a live session is the
  supported way to share one, and it is what the interface does.
- **A session is held in memory while an agent holds it.** Bounded at 32 idle sessions,
  least-recently-used first, and never at the cost of a running turn or an attached
  client. A session that is let go is still on disk and reloads on the next attach.
- **A client that attaches mid-turn is caught up with that turn, and no further.** The
  turn in flight crosses as one `Backlog` frame — the prompt, the steps, the deltas so far,
  and any question the turn is waiting on — and the live turn continues from there. The
  frames *inside* the batch are folded (adjacent deltas joined), so a client that was
  attached all along and one that arrived late hold the same text but not necessarily the
  same number of frames.
- **One agent, one thread.** A service serves several clients and their turns interleave
  cooperatively, because the kernel is single-threaded and its futures are not `Send`.
  Concurrency is not parallelism, and there is no worker pool. A step's tool calls do run
  together — cooperatively, capped by `max_parallel_tools`, and interleaved at their await
  points — but a call that blocks the thread blocks all of them.
- **Four providers, with one plan refused.** `deepseek`, `zai` (API and coding plans),
  `anthropic`, and `openai` (API and coding plans) all run; OpenAI's ChatGPT
  *subscription* plan is listed and refused by name, because it needs an OAuth token and
  the Responses API. Anthropic's extended thinking is not requested — a tool-using turn
  requires the signed thinking blocks of the previous turn replayed, and the message
  model has no place for a signature — so `reasoning_effort` has no effect there; the
  absence is recorded rather than a plausible value.
- **Only the macOS keychain ships as a platform secret store.** `SecretPort` and the
  `SecretBackend` trait are the seam, and a `0600` file and the environment are the
  fallbacks that work everywhere, but a Linux Secret Service or a Windows Credential
  Manager store is an implementation that does not exist yet.
- **The z.ai and OpenAI coding plans are endpoints, not subscriptions.** z.ai's coding
  plan is the same key and protocol at a different host, which is what makes it work;
  OpenAI's coding plan is a coding model on the same API. A tier that needs its own
  authorisation flow does not.
- **The sandbox is reported, not OS-enforced.** `nanus-adapter-local` checks the working
  directory and returns the policy, but installs no OS-level confinement. See
  [SAFETY.md](../SAFETY.md).

## Where the research lives

The dependency research, the process-execution measurements, the raw crates.io responses
and the probe workspaces that produced them live in a separate repository —
**`antstanley/nanus-research`** (private). They are evidence rather than shipped code, and
carrying them here meant a clone of the product also fetched research scratch.

One report stays in this repository, because it documents the framework the kernel
*implements* rather than the research that led to it:
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
a `LocalSet` *and* runs the runtime. Entering a local set without running it leaves every
spawned task un-polled, which looks exactly like a hung agent.
