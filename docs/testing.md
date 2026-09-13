# Testing and verification

Every number here is reproducible from a clean checkout. Nothing in this page is a
claim about intent; each line is the output of a command.

```console
$ cargo fmt --all --check
clean

$ cargo clippy --workspace --all-targets --all-features
0 warnings, 0 errors

$ cargo nextest run --workspace --all-features
Summary [3.4s] 605 tests run: 605 passed, 0 skipped

$ cargo test --workspace --doc
10 doctests passed
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

**The link, over real sockets.**
[`crates/nanus-link/tests/link.rs`](../crates/nanus-link/tests/link.rs) binds real sockets
in a temporary directory and drives a **scripted** model through a whole turn: the
handshake, a streamed answer, the ending, and the session on disk — plus the parts most
likely to be wrong and least likely to be covered by a unit test, which are the negatives.
Two connections get two sessions. A `status` request changes nothing. A `shutdown` request
stops a server that has no other stop condition, which is the test that would hang rather
than fail if the protocol were ignored. A socket left by a dead process is replaced, its
permissions are the owner's alone, and connecting to one nobody serves names the path
rather than reporting a syscall.

**The live path.** The harness has been run against the real DeepSeek API and the
resulting session log inspected: a four-step turn with tool calls, results fed back, and
per-step token accounting.

## The bugs verification found

All of them were found *because* the thing was exercised rather than read, and all of them
are now pinned by tests. They are recorded because a project claiming rigour should show
what rigour caught.

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

**Submitting a prompt aborted the process.** The turn runs as a local task, because the
kernel's state is `Rc`-shared and its futures are not `Send` — `tokio::spawn` cannot carry
them, so `spawn_local` is the only option, and it panics outside a `LocalSet`. Nothing had
ever entered one. The code dated from the first commit and had never executed: every test
covered the view, none covered the turn, and the interface had only ever been *read from*.
Reported by the first person to type into it. The loop is now driven by `block_on_local`,
which enters a local set *and* runs the runtime so the spawned task is polled — entering
alone would leave the task un-polled — and the loop is async, because awaiting a keystroke
synchronously would stall the very turn the local task exists to keep moving. A test now
submits a prompt against a scripted model and waits for the answer.

**The newest line could not be scrolled to.** A scroll offset counts display rows: a
reader scrolls rows, and the viewport is measured in rows. The transcript was counted in
logical *lines*, and the renderer wraps. Every wrapped line therefore put the bottom out of
reach by exactly the rows it wrapped into — so opening a session stopped short of its end,
and a notice appended to a long transcript landed below the fold, invisible. Both units are
now rows, and the tail is anchored to the bottom of the viewport, since a wrapped final
line cannot be drawn in part. Found by fixing the bug above and then noticing that the
feedback it produced was not on screen.

That second one was found by a model, in a live run, which then warned the user about it
in its answer. Which is the point of building a harness small enough to reason about.
