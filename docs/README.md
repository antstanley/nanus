# Documentation

The [README](../README.md) is the front door: what `nanus` is, why it exists, and how to
run it. Everything below is the detail behind that pitch.

## [The interface](tui.md)

Starting the interface, browsing a recorded session without an API key, the key bindings,
and the rendering choices — dimmed reasoning, paired tool calls, summarised output.
Includes a captured screenshot.

Start here if you want to see it working.

## [Design decisions](design.md)

One section per deliberate choice — safe Rust as a hard constraint, the seven-tool
toolset and why the count is the design, the three-field wire allowlist enforced by the
type system, fail-closed approval, the turn budget, and why the retired model ids have no
aliases. Each explains the reasoning rather than asserting a preference, and each says
which parts are deliberate divergences from the reference harness.

Start here if you are deciding whether to use this.

## [Architecture](architecture.md)

The crate graph, the inward-pointing dependency rule, what each crate owns, and how the
kernel's two halves — revertible effects and reactive coeffects — fit together.

Start here if you are going to read or change the code.

## [Testing and verification](testing.md)

The four quality gates and their current output, the two tests that matter most, and an
honest account of the four bugs that verification found rather than reasoning.

Start here if you want to know whether any of this is true.

## [Status](status.md)

What is complete, what is verified against the real API, and what is explicitly not
covered. Also the composition staging rule, which is subtle and was learned the hard way.

Start here if you are about to depend on something.

## [Style](style.md)

The Tiger Style rules, which of them are enforced by a tool rather than by review, and how
assertions and tests are written.

Start here if you are sending a patch.
