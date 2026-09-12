<div align="center">

# nanus

**A coding agent you can take apart.**

Ten crates of safe Rust. A seven-tool toolset. Every part of it — the model adapter,
the tool registry, the session log, the permission policy, and the agent loop itself —
is a plugin you can remove, replace, or write yourself.

[![tests](https://img.shields.io/badge/tests-520%20passing-brightgreen)](#proof-not-promises)
[![clippy](https://img.shields.io/badge/clippy-0%20warnings-brightgreen)](#proof-not-promises)
[![unsafe](https://img.shields.io/badge/unsafe-forbidden-blue)](#safe-rust-because-the-model-is-writing-the-code)
[![rust](https://img.shields.io/badge/rust-1.98-orange)](rust-toolchain.toml)
[![license](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

</div>

---

```console
$ nanus run "Use the glob tool to list the top-level Rust files, then tell me how many crates are in this workspace."

## Answer

**There are 10 crates in this workspace.**

There are **none** at the top level. I ran `glob` with `*.rs`, and it returned matches
— but every one of them is nested under `crates/`. The root is a virtual manifest, so
it has no source files of its own.

### The 10 crates

`cargo metadata --no-deps` confirms 10 packages, matching the 10 `members` entries in
the root `Cargo.toml` — the counts agree three ways.
```

That transcript is real output, unedited. The interesting part is what came *before*
the answer: four steps, seven tool calls, and the model noticing on its own that
`glob`'s pattern wasn't anchored where it expected — a real bug in this repository that
a test suite had missed and a model found.

---

## The pitch

Most agent harnesses are a loop, a pile of tools, and a config file that grew until it
became an API. You can use them. You cannot easily *change* them, because the parts
know about each other and the seams are in the wrong places.

`nanus` starts from the opposite end. There is no privileged core to patch. There is a
context, and there are components that register into it. A component declares what it
needs. The runtime decides when it runs. When a component goes away, everything it
touched goes away with it.

That gives you three things that are hard to get any other way:

**Replace the model without touching the tools.** The adapter is a plugin behind an
`LlmPort`. Swapping DeepSeek for anything else is one provider, and the seven tools
never learn about it.

**Replace the tools without touching the model.** Each tool is a plugin behind a
`ToolExecutor`. Add one and its schema joins the prompt automatically. Remove one and
it stops existing, with no dead description left in every request.

**Unload a component and get your system back.** Every registration is recorded *with
its inverse*. Unloading a plugin reverts its effects in reverse order: services
withdraw, listeners unregister, consumers deactivate. No stale registrations, no
restart to get clean, no "did the reload work?" There is a test for it, and the test
asserts on the trace order.

That last property has a name in the literature — *temporal composability* — and it is
the reason this project exists rather than being another loop in a `main.rs`.

## Why it is called nanus

A chain of names, each one smaller than the last.

**[Cordis](https://github.com/cordiverse/cordis)** is the meta-framework underneath,
from the Latin *cor, cordis* — **heart**. The name is a declaration of intent: the
framework is meant to be the organ everything else depends on, not a component among
them.

From *cor* comes **[Corvidae](https://en.wikipedia.org/wiki/Corvidae)** — the crows,
ravens, and jays. Among the most intelligent birds alive, and the only non-human
animals known to make hooks, use tools, and plan for a future they cannot see. If you
are building something that makes tools and uses them, you could do worse than a
corvid for a mascot.

The smallest corvid in the world is the **[dwarf jay](https://en.wikipedia.org/wiki/Dwarf_jay)**,
*Cyanolyca nanus* — 20 centimetres, 40 grams, endemic to the pine-oak forests of
southern Mexico. It is a jay, so it is one of those tool-using, problem-solving birds.
It is just very small.

**`nanus`** is the species epithet, and Latin for *dwarf*.

So: a small thing from a family of tool-users, named for the framework it was built on.
That is the whole idea. The smallest corvid that still uses tools.

> This is a tribute, not a claim — `nanus` is an independent project and borrows the
> framework's design, not its name's authority.

## What makes it different

Every one of these is a deliberate reading of the reference implementation, or a
deliberate divergence from it. They are listed because a reader comparing the two will
otherwise assume they are mistakes.

### Safe Rust, because the model is writing the code

`unsafe` appears **nowhere** in this repository. Every crate carries
`#![forbid(unsafe_code)]` *and* the workspace sets the lint — because the manifest lint
alone does not cover doctests, and a guarantee with an asterisk is not a guarantee.

This matters more than it sounds. A harness asks a language model to produce code that
will be compiled and run. The one thing you cannot afford is a memory-safety bug in the
layer that decides what to run.

The shell adapter needs to kill a whole process group. Without `unsafe`, that means
[`nix`](https://crates.io/crates/nix)'s safe wrappers rather than `libc` — which is
also, it turns out, the correct call for reasons that have nothing to do with safety
(see below).

### A shell tool that reaps its grandchildren

An agent harness runs commands as `sh -c "…"`, which means the process doing the work
is a **grandchild**. `kill_on_drop(true)` and `Child::kill()` do not reap
grandchildren. A killed tool leaves a process running behind it.

This was measured, not assumed: the experiment is reproducible and the finding is
recorded in the research repository. `nanus` spawns with `process_group(0)` and kills
the group. There is a test that reproduces the orphan and asserts it is gone.

A related trap found the same way: reading a subprocess pipe with
`AsyncReadExt::take(n)` **hangs** rather than truncating, because the writer blocks once
the 64 KiB pipe buffer fills. Output is read to end-of-file into a bounded buffer and
truncated after.

### Seven tools, and the count is the design

`read` · `write` · `edit` · `read_image` · `glob` · `grep` · `bash`

Each one is a mechanism a shell *cannot* provide as well — a bounded read window, an
exactly-once edit, a unified diff, a capped search that reports when it hit the cap.
Not one of them is a convenience wrapper around something `sh` already does.

There is deliberately no `list_directory`, no `move_file`, no `make_directory`, and no
per-language tool. Every tool costs a description in every request and a schema the
model must choose between. A tool earns its place by being irreplaceable, not by being
convenient.

### Only three fields of a tool can reach the model

A tool's `name`, `description`, and `parameters` may be serialised into a request. Its
executor, timeout, and presenters may not.

This is not a convention, it is a property of the types: the executable half is *not*
`Serialize`, so there is no code path by which a filesystem root, a sandbox policy, or a
session id can be encoded into a request body. The allowlist is carried by the compiler,
and a test asserts the serialised key set is exactly those three.

### Approval is fail-closed, and there is no "auto"

`ApprovalPolicy` is `Ask | Never`. `ApprovalOutcome` is
`AllowedOnce | Rejected | Cancelled | Unavailable`. Anything other than `AllowedOnce`
denies — a harness that cannot obtain an answer denies rather than proceeding.

`Never` deterministically rejects *without consulting anyone*, so a later-registered
policy cannot bypass it. There is deliberately no auto-approve mode, because a mode
that means "yes to everything" is a mode that ends with someone's `rm -rf` in a bug
report.

### A budget, because unattended loops are a cost hazard

The reference harness has **no** step budget: a tool loop continues until the model
stops calling tools. `nanus` bounds a turn. This is a deliberate divergence and is
documented at the type that enforces it.

### The current model ids, and no aliases for the dead ones

`deepseek-chat` and `deepseek-reasoner` were discontinued on **2026-07-24**. The
supported ids are `deepseek-flash` and `deepseek-v4-pro`, at
`https://api.deepseek.com` with no `/v1`.

Retired ids are deliberately **not** offered as aliases. Silently mapping a retired name
onto a new model would change a user's output without telling them, and a test asserts
the retired names do not resolve.

## Proof, not promises

Every number below is reproducible from a clean checkout.

```console
$ cargo fmt --all --check
clean

$ cargo clippy --workspace --all-targets --all-features
0 warnings, 0 errors

$ cargo nextest run --workspace --all-features
Summary [3.1s] 520 tests run: 520 passed, 0 skipped

$ cargo test --workspace --doc
9 doctests passed
```

The test that matters most is
[`crates/nanus-bundle/tests/end_to_end.rs`](crates/nanus-bundle/tests/end_to_end.rs):
it scripts a model calling the **real** tools over a temporary directory and asserts
**the file on disk changed**. A loop that asks a tool to write, and a file that was
written, are different claims — and only the second is the product working.

Alongside it, [`live_wire.rs`](crates/nanus-bundle/tests/live_wire.rs) runs the real
adapter against a local socket replaying genuine SSE frames, so tool-call reassembly is
tested over a real TCP connection rather than a mock stream.

### Two bugs found by verification rather than by reasoning

Both were found *because* the live path was exercised, and both are now pinned by tests.
They are recorded here because a project that claims rigour should show what rigour
caught.

**The `[DONE]` sentinel silently dropped every tool call.** The end-of-stream sentinel
ended the byte-reading loop without closing the accumulator — and the accumulator is
what emits assembled tool calls and usage. Tool-call arguments arrive as fragments, so
they *cannot* be emitted until the stream ends. Every live tool call was being discarded
and the loop saw an empty turn. A plain "say hi" worked perfectly, which is exactly why
it survived until a run that actually called a tool.

**`glob`'s pattern was not anchored.** `globset` lets `*` cross a directory separator by
default, and the matcher tested absolute paths — so `*.rs` matched at every depth, and a
bare pattern matched *everything*. `*.rs` and `**/*.rs` were indistinguishable, which
meant a model could not express "top level only". Now `*.rs` is the root level,
`**/*.rs` is every depth, and `src/*.rs` stops at the separator.

That second one was found by a model, in a live run, which then warned the user about
it in its answer. Which is the point of building a harness small enough to reason about.

## Get started

Requires stable Rust (pinned in `rust-toolchain.toml`) and
[`cargo-nextest`](https://nexte.st).

```sh
git clone https://github.com/antstanley/nanus.git
cd nanus
cargo build --release

export DEEPSEEK_API_KEY=...

# One shot: print the answer and exit.
./target/release/nanus run "Summarise this repository."

# See the reasoning and every tool call as it happens.
./target/release/nanus --verbose run "Find the TODO comments and group them by file."

# No key needed for either of these.
./target/release/nanus config      # the effective configuration
./target/release/nanus sessions    # transcripts of everything you have run
```

**stdout carries the answer and nothing else.** Reasoning and tool activity go to
stderr. The exit code is part of the contract: `0` only for a completed turn, non-zero
otherwise, so a script can tell a finished run from a failed one without parsing output.

The interactive interface is `nanus-tui --features runtime`.

## The architecture

Dependencies point inward. The core knows nothing about HTTP, the filesystem, or a
terminal.

```
                          ┌──────────────────────────────┐
                          │   nanus-cli    nanus-tui     │   entry points
                          └───────────────┬──────────────┘
                                          │
                          ┌───────────────▼──────────────┐
                          │        nanus-bundle          │   composition:
                          │  toolset · agent loop · wire │   what exists, and
                          └───────────────┬──────────────┘   how it finds itself
                                          │
        ┌─────────────────────────────────┼─────────────────────────────────┐
        │                                 │                                 │
┌───────▼────────┐              ┌─────────▼─────────┐             ┌─────────▼────────┐
│ adapter-       │              │   nanus-ports     │             │   nanus-kernel   │
│ deepseek       │              │  LlmPort FsPort   │             │  context         │
│ adapter-local  │─────────────▶│  ShellPort        │◀────────────│  effects         │
│ adapter-store  │   implement  │  StorePort        │   use       │  coeffects       │
│ adapter-config │              │  ClockPort        │             │  services·events │
└────────────────┘              └─────────┬─────────┘             └──────────────────┘
                                          │
                                ┌─────────▼─────────┐
                                │   nanus-domain    │   pure: no tokio,
                                │  messages · tools │   no HTTP, no I/O
                                │  session · turn   │
                                └───────────────────┘
```

`nanus-domain` has no `tokio`, no `reqwest`, and no filesystem. That is not a stylistic
preference — it means the agent's decisions can be tested without a network, and it is
enforced by the manifest, not by review.

| Crate | What it owns |
|---|---|
| [`nanus-kernel`](crates/nanus-kernel) | The Cordis framework itself. A context of revertible effects and reactive coeffects, a typed service registry, five event dispatch modes, the plugin lifecycle. Depends on nothing in the tree but `tokio`. Documented as a standalone library. |
| [`nanus-domain`](crates/nanus-domain) | Messages, the tool contract, the append-only session log, prompt assembly, approval policy, the turn machine. Pure. |
| [`nanus-ports`](crates/nanus-ports) | The boundary. Five traits and the service keys that let a provider and a consumer agree. |
| [`nanus-adapter-deepseek`](crates/nanus-adapter-deepseek) | The model adapter: request encoding, SSE decoding, streaming, tool-call reassembly. |
| [`nanus-adapter-local`](crates/nanus-adapter-local) | Rooted filesystem, process-group shell, clamping clock. |
| [`nanus-adapter-store`](crates/nanus-adapter-store) | Atomic JSONL session persistence with time-ordered ids. |
| [`nanus-adapter-config`](crates/nanus-adapter-config) | TOML configuration with a real migration. |
| [`nanus-bundle`](crates/nanus-bundle) | The toolset, the agent loop, and the one place that names concrete adapters. |
| [`nanus-cli`](crates/nanus-cli) · [`nanus-tui`](crates/nanus-tui) | A headless entry point and a ratatui interface. |

### How the two halves fit

The kernel gives you *spatiotemporal composability*, and the two words are worth
unpacking because they are the actual mechanism:

- **Temporal** — every mutation is recorded with its inverse. Unloading a component
  returns the system to its prior state. Tested by asserting the revert order.
- **Spatial** — a component declares the services it needs (its *coeffects*), and the
  runtime activates it when they appear and deactivates it when they vanish. Load order
  is a dependency, never a boot script.

Together they are why "everything is a plugin" is a mechanism here rather than a
slogan. The agent loop itself is a plugin. So you can replace it.

The design follows _A Programming Paradigm for Spatiotemporal Composability_
([arXiv:2608.25512](https://arxiv.org/abs/2608.25512)) and the reference harness
[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness). Where the paper and
the shipped framework disagree, [the mechanisms report](cordis-mechanisms-report.md)
records which one this kernel follows and why.

## Style

Tiger Style, enforced rather than aspired to:

- **`unsafe` forbidden**, in every crate and in the workspace — because the manifest
  lint does not cover doctests.
- **No `panic!`, `unwrap`, or `expect` in production code.** `assert!` is the sanctioned
  way to state an invariant, and invariants are asserted liberally: preconditions on
  entry, postconditions on exit, and *paired* assertions on both sides of a state change.
- **Arithmetic is explicit about overflow** — `checked_*` and `saturating_*` — because
  a token counter that wraps is a billing bug.
- **70 lines per function, 100 columns per line.**
- **No recursion**, and no panicking index arithmetic.
- **Errors are `Result`**, one `enum` per crate, with `From` impls translating vendor
  errors at the boundary. The domain never sees a `reqwest::Error`.

Tests name the behaviour, not the function, and every claim is tested in both
directions — the case that should work and the case that should fail. A test asserting
only the happy path is treated as incomplete.

## Status

Working, end to end. Progress is honest rather than flattering:

| | |
|---|---|
| Kernel, domain, ports, all four adapters | complete |
| Toolset, agent loop, composition | complete |
| CLI and TUI | complete |
| Live path (streaming, tool calls, results fed back) | **verified against the real API** |
| Interactive TUI | view layer tested headlessly; raw-mode input needs a real terminal |

Composing a harness is `compose(&config).await` for the adapters, then
`Pending::start()` outside the runtime for the kernel — the two phases exist because
`block_on` cannot be called from inside a runtime, and the split is enforced by types
rather than by remembering.

## The research

The dependency research, the process-execution measurements, the raw crates.io
responses, and the probe workspaces live in a separate private repository, because they
are evidence rather than shipped code:
**`antstanley/nanus-research`** (private).

The one report that stays here is
[cordis-mechanisms-report.md](cordis-mechanisms-report.md), because it documents the
framework this kernel implements rather than the research that led to it.

## References

- _A Programming Paradigm for Spatiotemporal Composability_ —
  [arXiv:2608.25512](https://arxiv.org/abs/2608.25512). Shi, Zhang, Cui (Peking
  University; DeepSeek-AI). The calculus behind the effect and coeffect mechanisms.
- [cordiverse/cordis](https://github.com/cordiverse/cordis) — the reference
  implementation, in TypeScript.
- [deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness) —
  the reference harness.
- [DeepSeek API documentation](https://api-docs.deepseek.com/) — the wire contract this
  implements.
- [Dwarf jay, *Cyanolyca nanus*](https://en.wikipedia.org/wiki/Dwarf_jay) — the smallest
  corvid, and the smallest bird in the family.

## License

MIT. See [SAFETY.md](SAFETY.md) before running it on a machine you care about — it
explains what the agent can do, what the defaults do and do not protect you from, and
why prompt injection is a real concern for anything that reads files.
