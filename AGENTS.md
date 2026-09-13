# AGENTS.md

Guidance for agents working in this repository. Read this before making changes;
the short version is that this codebase enforces its conventions with the
compiler, so most habits from other Rust projects will fail the build here.

## What this is

`nanus` is a coding-agent harness in safe Rust: a headless CLI and an interactive
TUI over a Rust implementation of the [Cordis](https://github.com/cordiverse/cordis)
meta-framework. Every part of it — the model adapter, the tool registry, the
session log, the permission policy, and the agent loop — is a plugin mounted on a
shared kernel context.

The front door for humans is [`README.md`](README.md). The docs in
[`docs/`](docs/README.md) explain *why* things are the way they are, and agents
should read the relevant page before changing a subsystem:

| Page | Read it when |
|---|---|
| [`docs/architecture.md`](docs/architecture.md) | Touching crate boundaries or the plugin/kernel model. |
| [`docs/design.md`](docs/design.md) | Changing a deliberate design decision (toolset size, approval, budget). |
| [`docs/testing.md`](docs/testing.md) | Adding tests or wondering what "verified" means here. |
| [`docs/status.md`](docs/status.md) | Depending on something; includes known limits. |
| [`docs/style.md`](docs/style.md) | Writing any Rust. |
| [`docs/tui.md`](docs/tui.md) | Changing the interface. |
| [`SAFETY.md`](SAFETY.md) | Anything that reads files, runs programs, or handles secrets. |

There is also [`cordis-mechanisms-report.md`](cordis-mechanisms-report.md), a long
reference on the framework the kernel implements. It is background, not
instructions.

## Repository layout

Ten crates in a Cargo workspace. Dependencies point **inward**; this is enforced
by the manifests, not by review. `nanus-domain` has no `tokio`, no `reqwest`, and
no filesystem, so agent decisions can be tested without a network.

```
nanus-cli ─▶ nanus-tui
    └─────▶ nanus-bundle ─▶ adapters (deepseek, local, store, config)
                    │               │
                    ▼               ▼
              nanus-ports ◀──── nanus-kernel
                    │
                    ▼
              nanus-domain  (pure: no tokio, no HTTP, no I/O)
```

| Crate | Owns |
|---|---|
| `crates/nanus-kernel` | The Cordis framework: revertible effects, reactive coeffects, service registry, typed events, plugin lifecycle. Depends only on `tokio`; documented as a standalone library. |
| `crates/nanus-domain` | Messages, the tool contract, the append-only session log, prompt assembly, approval policy, the turn machine. Pure. |
| `crates/nanus-ports` | The boundary: port traits (`LlmPort`, `FsPort`, `ShellPort`, `StorePort`, `ClockPort`) and the service keys that let provider and consumer meet without sharing a value. No I/O. |
| `crates/nanus-adapter-deepseek` | Request encoding, SSE decoding, streaming, tool-call reassembly. |
| `crates/nanus-adapter-local` | Rooted filesystem, process-group shell, clamping clock. |
| `crates/nanus-adapter-store` | Atomic JSONL session persistence with time-ordered ids. |
| `crates/nanus-adapter-config` | TOML configuration with a real migration chain. |
| `crates/nanus-bundle` | The toolset, the agent loop, and the **only** place that names concrete adapters. |
| `crates/nanus-cli` | The `nanus` binary: `run`, `tui`, `config`, `sessions`. |
| `crates/nanus-tui` | The ratatui interface as a library (view, input buffer, replay, event loop). |

## Toolchain and setup

- Stable Rust, pinned in [`rust-toolchain.toml`](rust-toolchain.toml) (1.98),
  with `rustfmt` and `clippy` components.
- Tests are run with [`cargo-nextest`](https://nexte.st). Install it before
  running the suite.
- No `DEEPSEEK_API_KEY` is needed to build, test, lint, or read recorded
  sessions. It is only needed to actually call the model (`nanus run`, live
  `nanus tui`).

## Commands

`default-members` is `nanus-kernel` **only**, so a bare `cargo build` or
`cargo test` does not cover the workspace. Always pass `--workspace` (or `-p
<crate>`) when you mean the whole project.

```sh
# The four quality gates. All must pass and all are expected to be clean.
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features
cargo nextest run --workspace --all-features
cargo test --workspace --doc

# Build the single binary (headless and interactive are the same program).
cargo build --release

# Run a subset while iterating.
cargo nextest run -p nanus-bundle
cargo nextest run -p nanus-bundle end_to_end
```

Use the `ci` nextest profile (defined in [`.config/nextest.toml`](.config/nextest.toml))
for retry-and-fail-fast behaviour: `cargo nextest run --profile ci --workspace`.

The current baseline is 542 tests, 9 doctests, 0 clippy warnings. If you change
that number, note that a few prose files quote it (the README badge/transcript
and `docs/testing.md`); agents should not chase those numbers unless asked.

## Running the binary

```sh
export DEEPSEEK_API_KEY=...
cargo run -p nanus-cli -- run "Summarise this repository."
cargo run -p nanus-cli -- --verbose run "Find the TODO comments."
cargo run -p nanus-cli -- config       # no key needed
cargo run -p nanus-cli -- sessions     # no key needed
```

Contract to preserve:

- **stdout is the answer and nothing else.** Reasoning and tool activity go to
  stderr.
- **Exit code is meaningful:** `0` only for a completed turn; a failed run or an
  exhausted step budget is non-zero.
- A bare `nanus` starts the TUI when there is a terminal and prints usage when
  there is not. It never panics on a missing terminal.

## Configuration and environment

- Config file: `<platform config dir>/nanus/config.toml` (flat TOML, every field
  defaulted, `config_version` for migrations).
- `NANUS_CONFIG` — override the config file path.
- `DEEPSEEK_API_KEY` — provider key. Read from the environment on each use; it is
  **never** stored in `NanusConfig`, serialised, or rendered by `Debug`.
- `NANUS_HOME` — override the session-store home. Sessions live under
  `$NANUS_HOME/sessions/` (default: the platform config dir).

## Conventions you must follow

This project follows [Tiger Style](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md),
and the important rules are enforced by clippy with `-D warnings`, so violating
them fails the build:

- **`unsafe` is forbidden** in every crate and at the workspace level. There is
  none in the repository and there must never be. Where a syscall-like operation
  is needed (process-group signalling), use safe crates such as `nix`.
- **No `panic!`, `unwrap`, `expect`, `todo!`, `unimplemented!`, or `dbg!` in
  production code.** These are denied by clippy. In tests they are allowed by
  [`clippy.toml`](clippy.toml), because a panic is the assertion mechanism.
- **`assert!` is the sanctioned way to state an invariant.** Assertions stay in
  release builds. Assert preconditions and postconditions, and where a state
  changes, assert on both sides.
- **Arithmetic must be explicit about overflow** — use `checked_*` / `saturating_*`.
  `arithmetic_side_effects` and `integer_division` are denied.
- **Limits:** 70 lines per function, 100 columns per line. `too_many_lines` and
  `cognitive_complexity` warn.
- **No printing from libraries.** `print_stdout` / `print_stderr` are denied
  outside `nanus-cli`, which is the binary and owns its output contract.
- **Errors are `Result`**, one error `enum` per crate, with `From` impls
  translating vendor errors at the boundary. A `reqwest::Error` never reaches the
  domain.
- **`missing_docs` warns.** Every public item needs a doc comment; `# Errors`
  sections are conventional on fallible functions.
- Clippy runs `all`, `pedantic`, `nursery`, and `cargo` groups at warn level, so
  write idiomatic code and prefer the suggested forms.

When a lint cannot be satisfied, the house style is to `allow` it *by name, at
the narrowest site, with a comment saying why* — see the crate-level allows in
`nanus-kernel` and `nanus-bundle` for the pattern.

## Testing conventions

- Tests live in `#[cfg(test)] mod tests` for unit tests, and in each crate's
  `tests/` directory for integration tests.
- **Name tests for the behaviour, not the function** (e.g.
  `a_workspace_that_is_not_a_directory_is_refused`).
- **Test both directions.** Every claim should have the case that works *and* the
  case that fails. A happy-path-only test is treated as incomplete. The suite
  carries deliberate negative cases (path escaping the workspace, an ambiguous
  edit, non-UTF-8 reads, duplicate plugin ids, truncated session tails).
- The two tests that matter most, and the models to imitate when adding an
  end-to-end test:
  - `crates/nanus-bundle/tests/end_to_end.rs` — scripts a model against the real
    tools over a temp workspace and asserts the file on disk changed (not just
    that a tool was called).
  - `crates/nanus-bundle/tests/live_wire.rs` — runs the real DeepSeek adapter
    against a local TCP server replaying real SSE frames.
- [`crates/nanus-bundle/src/tests_support.rs`](crates/nanus-bundle/src/tests_support.rs)
  holds shared test doubles (`UnusedFs`, `UnusedShell`). Prefer extending those
  over inventing new stubs.
- Doctests are part of the gates: `cargo test --workspace --doc` must pass. Public
  API docs often carry runnable examples; keep them compiling.

## Critical gotcha: async/sync composition staging

The kernel drives plugin hooks with its own `block_on`, and **`block_on` panics
when called from inside a runtime** ("cannot start a runtime from within a
runtime"). Composition is therefore split in two phases, and the split is encoded
in types:

1. `compose(&config).await` — builds adapters (opens the store, etc.). Must run
   *inside* the runtime.
2. `Pending::start()` — mounts the kernel. Must run *outside* the runtime,
   synchronously.

This is why `nanus-cli` has `prepare()` (async) and `finish()` (sync), and why
`main` calls `block_on(cli::prepare()).and_then(cli::finish)`. If you add code
that mounts a kernel or calls `block_on`, keep it in the synchronous half. The
same rule applies to the TUI: the turn is driven with `block_on_local`, because
the kernel's state is `Rc`-shared and its futures are `!Send`, so `tokio::spawn`
cannot carry them.

## Invariants that are enforced by tests

These are load-bearing; changing them means changing the tests and usually the
design docs too.

- **Exactly seven tools:** `read`, `write`, `edit`, `read_image`, `glob`, `grep`,
  `bash`. There are assertions on this count and on the name list in
  `nanus-bundle`. The count is a design decision (see `docs/design.md`), not an
  accident.
- **Wire allowlist:** only a tool's `name`, `description`, and `parameters` may be
  serialised to the model. The executable half of a `ToolDefinition` is not
  `Serialize`, so this is a type-level guarantee; a test asserts the serialised
  key set.
- **Approval is fail-closed:** only `AllowedOnce` proceeds; `Ask | Never` are the
  only policies and there is deliberately no auto-approve.
- **Retired model ids do not resolve.** `deepseek-chat` and `deepseek-reasoner`
  are gone; the supported ids are `deepseek-flash` and `deepseek-v4-pro`.
- **Temporal composability:** unloading a plugin reverts its effects in reverse
  order. `nanus-kernel/tests/composition.rs` asserts the revert order, not just
  the end state.

## How to make common changes

**Add a tool.** Create `crates/nanus-bundle/src/tools/<name>.rs` following an
existing tool (e.g. `read.rs`), read arguments through
`crate::args::Arguments` so a bad argument is a model-visible `ToolOutcome::Failure`
rather than a panic, export the factory from `tools/mod.rs`, and register it in
`build_toolset` in `nanus-bundle/src/lib.rs`. You must also update the size-7
array type, the `assert_eq!(registry.len(), 7, ...)` postcondition, and the
name-list test — and you should seriously consider whether the tool meets the bar
in `docs/design.md` (a mechanism the shell cannot do as well, not a convenience).

**Add a model provider.** Implement `nanus_ports::LlmPort`, publish it under
`llm_key()` with a plugin like `PortProvider` in `nanus-bundle/src/compose.rs`, and
select it in `build_llm`. Nothing under `nanus-domain` or the tools should change
— if it does, the seam is being crossed.

**Add a port.** Define the trait and `Handle` alias plus a `*_key()` function in
`nanus-ports`, implement it in an adapter crate, and publish it as a plugin. Port
methods are written as ordinary functions returning `LocalBoxFuture` (not
`async fn`) so the trait stays dyn-compatible; follow the existing pattern
exactly.

**Add a plugin to the kernel graph.** Implement `nanus_kernel::Plugin`; declare
dependencies via `requirements()` instead of ordering mounts. Registrations made
through `MountContext::provide` are recorded as effects and are reverted on
unload. See `nanus-kernel/src/lib.rs` for a worked doctest.

**Write a new adapter.** Keep vendor errors inside the crate: define a crate
error `enum` with `From` impls and expose only port types. No `unsafe`, no
`unwrap` in production paths.

## Safety

`nanus` lets a model read and write files and run programs as the current user.
Before changing anything around the sandbox, approval, shell execution, or
secrets, read [`SAFETY.md`](SAFETY.md). Key points:

- The sandbox is **reported, not OS-enforced**; a shell command that writes
  outside the workspace root is outside the workspace root.
- The shell adapter spawns with `process_group(0)` and kills the group, because
  `sh -c` runs the real work in a grandchild that `kill_on_drop` cannot reap.
  Keep the process-group test passing.
- A non-zero exit from `bash` is a result, not a harness failure.
- Never log or serialise the API key; never let a secret reach a request body.

## Version control

The checkout has both Git (`.git/`) and Jujutsu (`.jj/`, co-located) state. Check
`git status` / `jj status` before assuming which is authoritative in your
environment. History is small and commit messages are prose: a subject that says
what changed and why, capitalised, no conventional-commit prefixes.

## Working agreement for agents

- Read the relevant `docs/` page and the surrounding code before editing; this
  repository rewards understanding the invariants over pattern-matching.
- Match the surrounding style. Comments here explain *why*, often at length;
  keep that voice and update the comment when you change the behaviour it
  describes.
- Run all four gates before considering a change done. Do not leave the baseline
  count or lint status worse.
- Prefer editing files over rewriting them, and keep diffs focused; unrelated
  reformatting obscures review.
