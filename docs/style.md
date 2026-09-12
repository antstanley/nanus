# Style

[Tiger Style](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md),
enforced rather than aspired to. Every rule below is checked by `cargo fmt`, `cargo
clippy` with `-D warnings`, or a test — so a violation fails the build rather than
surviving a review.

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
## Assertions rather than defensive returns

`assert!` is the sanctioned way to state an invariant, and invariants are asserted
liberally: preconditions on entry, postconditions on exit, and *paired* assertions on
both sides of a state change. The pairing is the point — for every property worth
enforcing, the code asserts it at the write site and re-validates it at the read site
that depends on it, so a failure points at the actual broken condition rather than at
its symptom.

Assertions stay in release builds. A harness that disables its own invariants when it
matters most has no invariants.

## What the tests assert

Tests name the behaviour, not the function, and every claim is tested in both
directions: the case that should work and the case that should fail. A test asserting
only the happy path is treated as incomplete, so the suite carries deliberate negative
cases — a path escaping the workspace, an edit that matches twice, a non-UTF-8 read, a
duplicate plugin id, a truncated session tail, a waterfall listener that never delegates.
