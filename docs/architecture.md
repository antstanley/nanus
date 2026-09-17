# Architecture

Dependencies point inward. The core knows nothing about HTTP, the filesystem, or a
terminal — and that is enforced by the manifests rather than by review. `nanus-domain`
has no `tokio`, no `reqwest`, and no filesystem, so the agent's decisions can be tested
without a network.

```
                        ┌───────────────────────┐        ┌───────────────────────┐
                        │       nanus-cli       │        │       nanus-tui       │
                        │  the core: one binary │        │  the interface: its   │
                        │  run·service·tui·…    │        │  own program, and no  │
                        └────┬─────────────┬────┘        │  agent in sight       │
                             │             │             └──────────┬────────────┘
                             │             │   ┌────────────────┐   │
                             │             └──▶│   nanus-link   │◀──┘
                             │                 │ protocol·client│
                             │                 │    · server    │
                             │                 └────────────────┘
                        ┌────▼───────────────────────────┐
                        │          nanus-bundle          │  composition:
                        │  toolset · agent loop · wiring │  what exists, and
                        └───────────────┬────────────────┘  how it finds itself
                                        │
      ┌─────────────────────────────────┼─────────────────────────────────┐
      │                                 │                                 │
┌─────▼──────────┐            ┌─────────▼─────────┐             ┌─────────▼────────┐
│  adapter-      │            │    nanus-ports    │             │   nanus-kernel   │
│  deepseek      │            │                   │             │                  │
│  adapter-local │───────────▶│  LlmPort  FsPort  │◀────────────│  context         │
│  adapter-store │ implements │  ShellPort        │      uses   │  effects         │
│  adapter-config│            │  StorePort        │             │  coeffects       │
└────────────────┘            │  ClockPort        │             │  services        │
                              └─────────┬─────────┘             │  typed events    │
                                        │                       └──────────────────┘
                              ┌─────────▼─────────┐
                              │   nanus-domain    │  pure: no tokio,
                              │  messages · tools │  no HTTP, no I/O
                              │  session · turn   │
                              └───────────────────┘
```

Two binaries, and the arrow between them is a socket rather than a function call. That is
the load-bearing structural decision on this page: the core does not link the interface,
so the interface can grow without the thing a script runs growing with it. [`nanus-link`]
is the only vocabulary the two share.

[`nanus-link`]: ../crates/nanus-link

## What each crate owns

| Crate | What it owns |
|---|---|
| [`nanus-kernel`](../crates/nanus-kernel) | The Cordis framework itself: a context of revertible effects and reactive coeffects, a typed service registry, five event dispatch modes, the plugin lifecycle. Depends on nothing in the tree but `tokio`, and is documented as a standalone library. |
| [`nanus-domain`](../crates/nanus-domain) | Messages, the tool contract, the append-only session log, prompt assembly, approval policy, the turn machine. Pure. |
| [`nanus-ports`](../crates/nanus-ports) | The boundary: five traits and the service keys that let a provider and a consumer agree without sharing a value. |
| [`nanus-adapter-deepseek`](../crates/nanus-adapter-deepseek) | Request encoding, SSE decoding, streaming, tool-call reassembly. |
| [`nanus-adapter-local`](../crates/nanus-adapter-local) | Rooted filesystem, process-group shell, clamping clock. |
| [`nanus-adapter-store`](../crates/nanus-adapter-store) | Atomic JSONL session persistence with time-ordered ids, and the names sessions are known by. |
| [`nanus-adapter-config`](../crates/nanus-adapter-config) | TOML configuration with a real migration. |
| [`nanus-bundle`](../crates/nanus-bundle) | The toolset, the agent loop, and the one place that names concrete adapters. |
| [`nanus-link`](../crates/nanus-link) | The local link: the frame vocabulary, the Unix-socket transport, the client an interface uses, and — behind a `server` feature — the half that serves an agent and holds its sessions open. |
| [`nanus-cli`](../crates/nanus-cli) | The `nanus` binary: `run`, `service`, `config`, `sessions`, and the shell-scoped agent behind `tui`. It does not depend on `nanus-tui`. |
| [`nanus-tui`](../crates/nanus-tui) | The interface: the view, the input buffer, replay, and the terminal event loop. Its own binary (`nanus-tui`), a library for the parts that are testable without a terminal, and no dependency on the agent loop, a toolset, or a provider adapter — it links only the session store it reads recordings from and the configuration adapter it reads its own display preferences from. |

## Why the interface is a separate program

An earlier revision had one binary that served both. It read well — one configuration
loader, one workspace, no protocol — and it meant the core's dependency set included
`ratatui`, `crossterm`, and the whole view layer, for a program whose other modes are
`nanus run` in a shell script.

The honest version of that trade is that a function call is a better seam than a socket,
and the reason to pay for the socket anyway is that the two halves grow at different
rates. The interface is the part that will grow: more panes, more keys, more rendering,
more of the things that make a terminal application worth using. The core is the part that
must not: it is what a boot script starts, what a pipeline invokes, and what a person
audits before letting it run commands. A seam that lets one grow without the other is
worth a protocol.

The cost is real and worth naming: a socket can be down, a version can differ, and a
frame can be lost. The first is handled by saying so, the second by shipping the two
binaries together and never searching `PATH`, and the third by making the session on disk
the authority — the transcript survives an interface that missed a frame, because the
agent recorded it.

## How the two halves fit

The kernel gives you *spatiotemporal composability*, and the two words are worth
unpacking because they are the actual mechanism:

- **Temporal** — every mutation is recorded with its inverse. Unloading a component
  returns the system to its prior state. Tested by asserting the revert order.
- **Spatial** — a component declares the services it needs (its *coeffects*), and the
  runtime activates it when they appear and deactivates it when they vanish. Load order
  is a dependency, never a boot script.

Together they are why "everything below the loop is a plugin" is a mechanism here rather
than a slogan: the ports and the tool registry are components the runtime activates, and the
loop is built over the handles they publish. The loop itself is not one of them — it is
handed to its caller rather than provided as a service, so replacing it is an edit to the
composition. [The design decisions](design.md#the-two-halves-and-why-they-are-the-mechanism)
say why that gap exists and what closing it would take.

The design follows _A Programming Paradigm for Spatiotemporal Composability_
([arXiv:2608.25512](https://arxiv.org/abs/2608.25512)) and the reference harness
[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness). Where the paper and
the shipped framework disagree, [the mechanisms report](../cordis-mechanisms-report.md)
records which one this kernel follows and why.
