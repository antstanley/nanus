# nanus

A headless CLI and TUI agent harness in Rust, built on a Rust reimplementation of
the [Cordis](https://github.com/cordiverse/cordis) meta-framework.

`nanus` is a small, inspectable agent harness. It supports DeepSeek models, gives
the model a deliberately minimal toolset, and composes every part of itself —
including the agent loop — as a plugin that can be replaced from configuration.

## Why a meta-framework

The design follows _A Programming Paradigm for Spatiotemporal Composability_
([arXiv:2608.25512](https://arxiv.org/abs/2608.25512)), the paper behind Cordis,
and the reference harness [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness).
Two properties are the whole point:

- **Temporal composability** — every mutation a component makes to shared state is
  recorded together with its inverse, so unloading a component returns the system
  to the state it had before. There is no partial teardown and no reload that
  leaves a stale registration behind.
- **Spatial composability** — a component declares the capabilities it needs and
  the runtime decides *when* it runs, activating it when those capabilities appear
  and deactivating it when they vanish. Load order is a dependency, never a boot
  script.

Those two properties are what make "everything is a plugin" more than a slogan:
the model adapter, the tool registry, the session log, the permission policy, and
the agent loop are all mounted on the same context, and any of them can be
replaced without patching a core.

`crates/nanus-kernel` is that framework, and it is the only part of the tree that
is not about agents. It is documented as a standalone library.

## Status

Working, end to end. Every crate builds, the suite is green, and the shipped
toolset has been exercised against a real filesystem and a real shell. See
[Implementation status](#implementation-status) for the evidence.

## Layout

The tree is hexagonal. Dependencies point inward: the core knows nothing about
HTTP, the filesystem, or a terminal.

```
crates/
  nanus-kernel            the Cordis meta-framework: context, effects,
                          coeffects, services, typed events, plugin lifecycle
  nanus-domain            pure agent domain: messages, tools, the session log,
                          prompt assembly, approval policy, the turn machine
  nanus-ports             the hexagon's boundary: LlmPort, FsPort, ShellPort,
                          StorePort, Clock
  nanus-adapter-deepseek  the model adapter (the only provider, DeepSeek)
  nanus-adapter-local     filesystem and process execution
  nanus-adapter-store     append-only session persistence
  nanus-adapter-config    configuration file loading and migration
  nanus-bundle            the plugin set that assembles a working harness
  nanus-cli               the headless entry point
  nanus-tui               the interactive terminal UI
```

### The dependency rule

| Layer | May depend on | Must never depend on |
|---|---|---|
| `nanus-domain` | the standard library, `serde` | `tokio`, `reqwest`, the kernel, any adapter |
| `nanus-ports` | `nanus-domain` | any adapter |
| `nanus-adapter-*` | `nanus-ports`, vendor crates | the CLI, the TUI, each other |
| `nanus-kernel` | the standard library only | the domain, the ports, any adapter |
| `nanus-bundle` | everything above | the CLI, the TUI |
| `nanus-cli`, `nanus-tui` | `nanus-bundle` | adapters directly |

`nanus-kernel` depends on nothing in the tree, which is what lets the framework be
read and understood on its own.

## Design decisions worth knowing

Each of these is a deliberate divergence from, or a deliberate reading of, the
reference implementation. They are listed because a reader comparing the two will
otherwise assume they are mistakes.

### Only DeepSeek, and the current model ids

`deepseek-chat` and `deepseek-reasoner` were discontinued on **2026-07-24**.
The supported ids are `deepseek-flash` and `deepseek-v4-pro`, and the base URL is
`https://api.deepseek.com` with no `/v1` prefix. Retired ids are deliberately *not*
offered as aliases: silently mapping a retired name onto a new model would change
a user's output without telling them.

### Single-threaded, like Cordis

The kernel is single-threaded. Components are `Rc`-shared rather than
`Arc`-shared, futures need not be `Send`, and dispatch order is registration order.
That is not a simplification for convenience — it is what makes teardown order a
property the runtime can *guarantee*, and therefore what makes temporal
composability real rather than aspirational.

### A turn budget exists

The reference harness has **no built-in step budget**: a tool loop continues until
the model stops calling tools. `nanus` adds one (`max_steps_per_turn`, default 16)
because an unattended harness with an unbounded loop is a cost hazard. This is a
deliberate divergence and is documented at the type that enforces it.

### Approval is fail-closed, and there is no `auto`

`ApprovalPolicy` is `Ask | Never` and `ApprovalOutcome` is
`AllowedOnce | Rejected | Cancelled | Unavailable`. Anything other than
`AllowedOnce` denies. A harness that cannot obtain an answer denies rather than
proceeding. `Never` deterministically rejects without consulting any answerer, so
a later-registered policy cannot bypass it.

### The two permission knobs are orthogonal

Approval (`ask | never`) and sandbox mode (`read-only | workspace-write |
danger-full-access`) are separate settings. A "permission preset" bundles them for
presentation only; it is not a third mode, and `custom` is a derived label that can
never be selected.

### Only three fields of a tool reach the model

A tool's `name`, `description`, and `parameters` may be serialised into a request.
Its executor, timeout, and presenters must not be. This is an extension point — a
plugin can register a tool — so the allowlist is enforced at the wire boundary and
asserted by a test rather than left to review.

### Running a shell means killing a process group

An agent harness runs commands as `sh -c "..."`, which means the process doing the
work is a **grandchild**. `kill_on_drop(true)` and `Child::kill()` do not reap
grandchildren; a killed tool can leave a process running behind it. `nanus`
therefore spawns with `process_group(0)` and kills the group, using `nix`'s safe
wrappers so the tree keeps `unsafe_code = "forbid"`. There is a test that
reproduces the orphan and asserts it is gone.

A related trap: reading a subprocess pipe with `AsyncReadExt::take(n)` **hangs**
rather than truncating, because the writer blocks once the 64 KiB pipe buffer
fills. Output is read to end-of-file into a bounded buffer and truncated after.

## Building

Requires the stable Rust toolchain (pinned in `rust-toolchain.toml`) and
`cargo-nextest`.

```sh
cargo build --workspace
cargo nextest run --workspace
cargo fmt --all
cargo clippy --workspace --all-targets
```

`nextest` does not run doctests. Run them separately:

```sh
cargo test --doc
```

## Style

Tiger Style, as the constraints require. In practice, in this tree:

- **`unsafe` is forbidden**, by both a crate-root `forbid` attribute and the
  workspace lint, because the manifest lint alone does not cover doctests.
- **No `panic!`, `unwrap`, or `expect` in production code.** `assert!` is the
  sanctioned way to state an invariant, and invariants are asserted liberally —
  preconditions on entry, postconditions on exit, and paired assertions on both
  sides of a state change.
- **Arithmetic is explicit about overflow**, via `checked_*` and `saturating_*`.
  Overflow in a token counter or a sequence number must not wrap.
- **70 lines per function, 100 columns per line.**
- **Errors are `Result`**, one `thiserror` enum per crate, with `From` impls
  translating vendor errors at the boundary. `nanus-domain` never sees a
  `reqwest::Error` or a `std::io::Error`.
- **No recursion**, and no panicking index arithmetic — `.get()` everywhere the
  index is not already proven in range.

Tests name the behaviour, not the function, and every claim is tested in both
directions: the case that should work and the case that should fail. A test that
only asserts the happy path is treated as incomplete.

## Implementation status

| Component | State |
|---|---|
| `nanus-kernel` | Complete: context, revertible effects, reactive coeffects, services, typed events, plugin lifecycle. |
| `nanus-domain` | Complete: messages, the tool contract, the session log, prompt assembly, approval, the turn machine. |
| `nanus-ports` | Complete: `LlmPort`, `FsPort`, `ShellPort`, `StorePort`, `ClockPort` and the shared service keys. |
| `nanus-adapter-deepseek` | Complete: request encoding, SSE decoding, streaming, tool-call reassembly. |
| `nanus-adapter-local` | Complete: rooted filesystem, process-group shell, clamping clock. |
| `nanus-adapter-store` | Complete: atomic JSONL sessions with time-ordered ids. |
| `nanus-adapter-config` | Complete: TOML schema, four-level precedence, migration. |
| `nanus-bundle` | Complete: the seven-tool toolset, the agent loop, the composition. |
| `nanus-cli` | Complete: `run`, `config`, `sessions`. |
| `nanus-tui` | Complete: the view layer and the interactive loop. |

### Evidence

Measured on the pinned toolchain, from a clean checkout:

```text
cargo fmt --all --check                      clean
cargo clippy --workspace --all-targets      0 warnings, 0 errors
  --all-features
cargo nextest run --workspace --all-features
                                            508 tests, 508 passed
cargo test --workspace --doc                8 doctests, 8 passed
```

`unsafe` appears nowhere in the tree: every crate carries
`#![forbid(unsafe_code)]` *and* the workspace sets `unsafe_code = "forbid"`,
because the manifest lint alone does not cover doctests.

`crates/nanus-bundle/tests/end_to_end.rs` is the test that matters most: it
scripts a model that calls the real tools, composes the shipped toolset over a
temporary directory, and asserts the file on disk changed. A loop that asks a tool
to write and a file that was written are different claims, and only the second is
the product working.

## References

- _A Programming Paradigm for Spatiotemporal Composability_ —
  [arXiv:2608.25512](https://arxiv.org/abs/2608.25512), Yao Shi, Wei Zhang,
  Tianyi Cui (Peking University, DeepSeek-AI). The calculus behind the kernel's
  effect and coeffect mechanisms.
- [cordiverse/cordis](https://github.com/cordiverse/cordis) — the reference
  implementation of the framework, in TypeScript. `cordis-mechanisms-report.md` in
  this repository records the semantics `nanus-kernel` follows and the places the
  paper and the shipped code disagree.
- [deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness) —
  the reference agent harness. `nanus` follows its architecture where that
  architecture is a consequence of the framework, and diverges deliberately
  elsewhere.
- [DeepSeek API documentation](https://api-docs.deepseek.com/) — the wire contract
  implemented by `nanus-adapter-deepseek`.

## License

MIT.
