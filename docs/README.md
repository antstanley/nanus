# Documentation

The [README](../README.md) is the front door: what `nanus` is, why it exists, and how to
run it. Everything below is the detail behind that pitch.

## [Features](features.md)

A map of what the harness supports today — the three modes, the seven tools, the
provider and its controls, sessions, configuration, the command line, the interface, the
service, the link, the kernel, and what is deliberately not covered — each with a link to
the page that explains it.

Start here if you want to know what it can do before reading how.

## [The interface](tui.md)

Starting the interface, how it reaches an agent, browsing a recorded session without an
API key, the key bindings, and the rendering choices — dimmed reasoning, paired tool
calls, summarised output. Includes a captured screenshot, and the frame vocabulary and
trust boundary of the local link.

Start here if you want to see it working.

## [Sessions](sessions.md)

What a conversation is on disk, how it is named and renamed, how it is resumed, and what
it means to attach to one that is still running. Includes the store layout, the frames
involved, and the limits — a session is not locked, so two writers can lose a turn.

Start here if you want to come back to work tomorrow.

## [The service](service.md)

An agent that outlives the shell: starting it detached or under a supervisor, stopping it
without a signal, the socket and log it uses, and what a second service on one machine
looks like. Also the limits — a service is local, Unix-only, and trusts the user it runs
as.

Start here if you want an agent that is still there tomorrow.

## [Design decisions](design.md)

One section per deliberate choice — safe Rust as a hard constraint, the seven-tool
toolset and why the count is the design, the three-field wire allowlist enforced by the
type system, fail-closed approval, the turn budget, and why the retired model ids have no
aliases. Each explains the reasoning rather than asserting a preference, and each says
which parts are deliberate divergences from the reference harness.

Start here if you are deciding whether to use this.

## [Architecture](architecture.md)

The crate graph, the inward-pointing dependency rule, what each crate owns, why the
interface is a separate program rather than a library, and how the kernel's two halves —
revertible effects and reactive coeffects — fit together.

Start here if you are going to read or change the code.

## [Testing and verification](testing.md)

The quality gates and their current output, the tests that matter most — including
the link driven over real sockets — and an honest account of the bugs that verification
found rather than reasoning.

Start here if you want to know whether any of this is true.

## [Status](status.md)

What is complete, what is verified against the real API, and what is explicitly not
covered. Also the composition staging rule, which is subtle and was learned the hard way.

Start here if you are about to depend on something.

## [Roadmap](roadmap.md)

What is planned, in priority order, with a rough t-shirt size on each item: the
correctness gaps between what the docs claim and what runs, the interface features that
would make it a daily driver, the larger bets like an OS-enforced sandbox, and the
things that are deliberately absent.

Start here if you want to know what is coming, or why something is not.

## [Goal research note](goal-research.md)

A survey of how PrimeIntellect, OpenAI Codex, and DeepSeek Harness implement a persistent
objective (`/goal`), what they agree on, and what a goal should look like here — including
the tension between automatic continuation and the turn budget, and the questions left
open. Background for [roadmap item 24](roadmap.md#next-new-capabilities).

Start here if you are picking up the goal work, or designing a feature that spans turns.

## [Style](style.md)

The Tiger Style rules, which of them are enforced by a tool rather than by review, and how
assertions and tests are written.

Start here if you are sending a patch.
