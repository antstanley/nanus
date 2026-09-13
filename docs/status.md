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

- **Live tool calling was verified by inspection, not replay.** The framing replayed in
  `live_wire.rs` follows the documented shape, and a live text run confirmed the request
  side, TLS, auth, and the reasoning passback rule. What no hermetic test can prove is
  that the live API's tool-call frames match the replay byte for byte; if they differ,
  [`crates/nanus-adapter-deepseek/src/wire.rs`](../crates/nanus-adapter-deepseek/src/wire.rs)
  is the only file involved.
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
- **A client that attaches mid-turn sees the rest of it.** The frames before it went to
  clients that were already there. The transcript is still whole — the store is where
  history comes from — but a watcher joining late has a gap until the turn ends.
- **One agent, one thread.** A service serves several clients and their turns interleave
  cooperatively, because the kernel is single-threaded and its futures are not `Send`.
  Concurrency is not parallelism, and there is no worker pool.
- **DeepSeek is the only provider.** The `LlmPort` seam is real and a second adapter
  would be a single file, but none exists yet.
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
