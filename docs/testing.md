# Testing and verification

Every number here is reproducible from a clean checkout. Nothing in this page is a
claim about intent; each line is the output of a command.

```console
$ cargo fmt --all --check
clean

$ cargo clippy --workspace --all-targets --all-features
0 warnings, 0 errors

$ cargo nextest run --workspace --all-features
Summary [3.0s] 773 tests run: 773 passed, 0 skipped

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
truncated mid-frame, and the request carrying all seven schemas — and one case replays a
response **captured from the live API**
([`tests/data/`](../crates/nanus-bundle/tests/data/README.md)), a `bash` call whose
arguments arrived a few characters at a time, so the decoder is held to a shape nobody
here wrote rather than to the one this suite assumes.

**The link and its sessions, over real sockets.**
[`crates/nanus-link/tests/link.rs`](../crates/nanus-link/tests/link.rs) binds real sockets
in a temporary directory and drives a **scripted** model through whole turns: the
handshake, the attachment, a streamed answer, the ending, and the session on disk — plus the
parts most likely to be wrong and least likely to be covered by a unit test, which are the
negatives. A name another session holds is refused and creates nothing. Attaching to a
session that was never held loads it from the store. A **turn finishes after the client
that asked for it leaves**, and a second client that joins sees the ending. Two clients on
one session both see the turn, and the one that did not ask is told what was asked. A second
prompt while a turn runs is refused rather than queued. A client that attaches again leaves
no stale viewer behind — on either path, `new` or `attach` — and a session opened while
every other one is in use is never the one let go. A `status` request opens no session at
all. A `shutdown` request stops a server that has no other stop condition, which is the
test that would hang rather than fail if the protocol were ignored. A socket left by a dead
process is replaced, its permissions are the owner's alone, and connecting to one nobody
serves names the path rather than reporting a syscall.

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

## The bugs a structured review found

Two more came out of reviewing the session work line by line rather than running it, and
both are the kind that a green suite cannot see because nothing in the shipped clients
reaches them:

**Attaching again left the old session subscribed.** A connection that moved from one
session to another dropped its `(viewer, session)` pair without unsubscribing. The session
it left kept queueing its frames into a client that was watching something else — one
conversation's words in another's transcript — and, because it still counted as attached,
could never be let go. Both ways of moving had it, and both are now tested separately,
because fixing one arm of a two-arm bug is exactly the mistake the first version of the
test made: it exercised `new` and passed against a build with `attach` still broken.

**A session could evict itself.** Room was made for a new session *after* it was in the
registry, so when every other held session was in use the newcomer was the only eviction
candidate and was dropped immediately. The client was then handed a conversation the agent
no longer held — invisible to a listing, unreachable by a second client, and loaded a
second time by anyone who tried. Reaching it needs every other session attached, which is
why the first version of *that* test also proved nothing: the oldest idle session is
evicted first, so the test has to fill the registry before opening one more.

The lesson both times was the same, and it is about tests rather than code: a regression
test is only a regression test if it fails against the code it was written for. Each of
these was checked by reverting the fix and watching the test fail — and two of the four
did not, which is how the tests got rewritten.

## The bug a terminal found

One more, reported from a real terminal rather than found by anything here, and worth
recording because the cause was a whole class of defect rather than a typo.

**`Shift+Enter` typed a `j`.** On Ghostty, `shift+enter` is bound to *send a newline* — the
terminal types a line feed instead of reporting a key, so no keyboard protocol is involved.
In raw mode a line feed is `Ctrl+J`, and terminals report control bytes as `Ctrl+<letter>`.
The interface's key handler ended in a catch-all that inserted any character and looked at
no modifiers, so `Ctrl+J` inserted `j`. The same arm was inserting a letter for *every*
control key it had not claimed: `Ctrl+K` typed `k`, and `Ctrl+H` typed `h` for a byte that
is also backspace.

The reproduction was a two-line experiment rather than a guess: inject the bytes into a
running interface with `tmux send-keys -H` and watch what appears, and inject them into a
program that prints crossterm's parsed events to see the code and modifiers behind each.
Control bytes `0x0A`, `0x0B`, and `0x08` came back as `Char('j')`, `Char('k')`, and
`Char('h')`, each with `CONTROL` — which is exactly what was on screen.

Two changes, one line each: `Ctrl+J` starts a line, and a character with Control held is no
longer text. Alt is deliberately not guarded the same way, because on many terminals an
Option or Alt press arrives as `Alt+<letter>` on its way to producing a character, and
swallowing those would stop some keyboards typing at all.

The same investigation turned up a hazard this interface happens to avoid, and it is worth
recording because the next binding may not. A terminal that reports modifiers attaches
`SHIFT` to the characters those modifiers produce: `?` arrives as `Char('?')` *with*
`SHIFT`. A binding that compares whole key events then fails for exactly the keys someone
tests by pressing them, while `Shift-?` works — [ratatui/templates#26][shift-issue]. The key
handler here reads the key *code* and ignores modifiers when inserting text, so typing is
unaffected, and there is now a test that fails if that stops being true.

Asking the same question of the *control* bindings found one that was wrong, by injecting
the bytes a terminal sends for `Ctrl+C` with Caps Lock on: `Char('C')` with `CONTROL`.
Nothing happened — with the keyboard protocol enabled, the interface could not be quit and
its toggles were dead for anyone typing in capitals. The bindings now match either case.
Injection is what made this visible: the parsed event was already in hand, and the question
"what does the modifier do to the character" had simply not been asked of it.

[shift-issue]: https://github.com/ratatui/templates/issues/26
