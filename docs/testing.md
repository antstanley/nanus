# Testing and verification

Every number here is reproducible from a clean checkout. Nothing in this page is a
claim about intent; each line is the output of a command.

```console
$ cargo fmt --all --check
clean

$ cargo clippy --workspace --all-targets --all-features
0 warnings, 0 errors

$ cargo nextest run --workspace --all-features
Summary [2.7s] 539 tests run: 539 passed, 0 skipped

$ cargo test --workspace --doc
9 doctests passed
```

## The tests that matter most

**End to end, over the real tools.**
[`crates/nanus-bundle/tests/end_to_end.rs`](../crates/nanus-bundle/tests/end_to_end.rs)
scripts a model calling the **real** tools over a temporary directory and asserts **the
file on disk changed**. A loop that asks a tool to write, and a file that was written,
are different claims — and only the second is the product working. It also asserts the
negative space: a write outside the workspace is refused and nothing is created, and an
ambiguous edit leaves the file byte-for-byte unchanged.

**The wire, over a real socket.**
[`crates/nanus-bundle/tests/live_wire.rs`](../crates/nanus-bundle/tests/live_wire.rs)
runs the real adapter against a local TCP server replaying genuine SSE frames, so
tool-call reassembly is exercised over a real connection with a real HTTP client rather
than against a mock stream. It covers a tool call split across three frames, a response
truncated mid-frame, and the request carrying all seven schemas.

**The live path.** The harness has been run against the real DeepSeek API and the
resulting session log inspected: a four-step turn with tool calls, results fed back, and
per-step token accounting.

## Four bugs found by verification rather than by reasoning

All four were found *because* the thing was exercised rather than read, and all four are
now pinned by tests. They are recorded because a project claiming rigour should show what
rigour caught.

**The `[DONE]` sentinel silently dropped every tool call.** The end-of-stream sentinel
ended the byte-reading loop without closing the accumulator — and the accumulator is what
emits assembled tool calls and usage. Tool-call arguments arrive as fragments, so they
*cannot* be emitted until the stream ends. Every live tool call was discarded and the loop
saw an empty turn. A plain "say hi" worked perfectly, which is exactly why it survived
until a run that actually called a tool.

**`glob`'s pattern was not anchored.** `globset` lets `*` cross a directory separator by
default, and the matcher tested absolute paths — so `*.rs` matched at every depth, and a
bare pattern matched *everything*. `*.rs` and `**/*.rs` were indistinguishable, which
meant a model could not express "top level only". Now `*.rs` is the root level,
`**/*.rs` is every depth, and `src/*.rs` stops at the separator.

**The interface aborted when there was no terminal.** `ratatui::init` panics rather than
returning an error, so `nanus tui --session | cat` died from inside the drawing library
with exit 134 and a message about a device it could not configure. Found by capturing the
screenshot, which meant running the binary through `tmux` and then, by accident, outside
it. The terminal is now verified before it is taken, and the refusal is a pure function of
whether a terminal exists — so the guard is tested without a test that would itself take
over a terminal.

**A live conversation announced itself as a recording.** The transcript builder prefixed
every session with a header, so a conversation that had just started opened by saying
`recorded session · <untitled> · 0 events` — false, and stale the moment the first turn
arrived. Found by running a bare `nanus` in a real terminal to check that the merged
binary started the interface. A header belongs to reading a recording; a live conversation
needs none, because the reader is already in it. Building that banner is now a property of
the recording, not of the session.

That second one was found by a model, in a live run, which then warned the user about it
in its answer. Which is the point of building a harness small enough to reason about.
