# Architecture

Dependencies point inward. The core knows nothing about HTTP, the filesystem, or a
terminal — and that is enforced by the manifests rather than by review. `nanus-domain`
has no `tokio`, no `reqwest`, and no filesystem, so the agent's decisions can be tested
without a network.

```
                          ┌──────────────────────────────────┐
                          │            nanus-cli             │  one binary:
                          │   run · tui · config · sessions  │  headless and
                          └───────┬──────────────────┬───────┘  interactive
                                  │                  │
                                  │        ┌─────────▼─────────┐
                                  │        │    nanus-tui      │  the interface,
                                  │        │  view · replay    │  as a library
                                  │        └─────────┬─────────┘
                                  │                  │
                          ┌───────▼──────────────────▼───────┐
                          │          nanus-bundle            │  composition:
                          │   toolset · agent loop · wiring  │  what exists, and
                          └────────────────┬─────────────────┘  how it finds itself
                                           │
        ┌──────────────────────────────────┼──────────────────────────────────┐
        │                                  │                                  │
┌───────▼─────────┐              ┌─────────▼─────────┐              ┌─────────▼────────┐
│  adapter-       │              │    nanus-ports    │              │   nanus-kernel   │
│  deepseek       │              │                   │              │                  │
│  adapter-local  │─────────────▶│  LlmPort  FsPort  │◀─────────────│  context         │
│  adapter-store  │  implements  │  ShellPort        │      uses    │  effects         │
│  adapter-config │              │  StorePort        │              │  coeffects       │
└─────────────────┘              │  ClockPort        │              │  services        │
                                 └─────────┬─────────┘              │  typed events    │
                                           │                        └──────────────────┘
                                 ┌─────────▼─────────┐
                                 │   nanus-domain    │  pure: no tokio,
                                 │  messages · tools │  no HTTP, no I/O
                                 │  session · turn   │
                                 └───────────────────┘
```

## What each crate owns

| Crate | What it owns |
|---|---|
| [`nanus-kernel`](../crates/nanus-kernel) | The Cordis framework itself: a context of revertible effects and reactive coeffects, a typed service registry, five event dispatch modes, the plugin lifecycle. Depends on nothing in the tree but `tokio`, and is documented as a standalone library. |
| [`nanus-domain`](../crates/nanus-domain) | Messages, the tool contract, the append-only session log, prompt assembly, approval policy, the turn machine. Pure. |
| [`nanus-ports`](../crates/nanus-ports) | The boundary: five traits and the service keys that let a provider and a consumer agree without sharing a value. |
| [`nanus-adapter-deepseek`](../crates/nanus-adapter-deepseek) | Request encoding, SSE decoding, streaming, tool-call reassembly. |
| [`nanus-adapter-local`](../crates/nanus-adapter-local) | Rooted filesystem, process-group shell, clamping clock. |
| [`nanus-adapter-store`](../crates/nanus-adapter-store) | Atomic JSONL session persistence with time-ordered ids. |
| [`nanus-adapter-config`](../crates/nanus-adapter-config) | TOML configuration with a real migration. |
| [`nanus-bundle`](../crates/nanus-bundle) | The toolset, the agent loop, and the one place that names concrete adapters. |
| [`nanus-cli`](../crates/nanus-cli) | The `nanus` binary, and the only entry point: `run` and `tui` both live here, because two binaries would mean two argument parsers, two configuration loads, and two answers to which workspace a session belongs to. |
| [`nanus-tui`](../crates/nanus-tui) | The ratatui interface as a library: the view, the input buffer, replay of a recorded session, and the terminal event loop behind a `runtime` feature. It is a library rather than a program, so there is exactly one thing to run. |

## How the two halves fit

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
the shipped framework disagree, [the mechanisms report](../cordis-mechanisms-report.md)
records which one this kernel follows and why.