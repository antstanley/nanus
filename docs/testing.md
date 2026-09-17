# Testing and verification

Every number here is reproducible from a clean checkout. Nothing in this page is a
claim about intent; each line is the output of a command.

```console
$ cargo fmt --all --check
clean

$ cargo clippy --workspace --all-targets --all-features
0 warnings, 0 errors

$ cargo nextest run --workspace --all-features
Summary [5.3s] 918 tests run: 918 passed, 0 skipped

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


## The bugs a certificate review found

A later pass over the whole tree — every crate read in dependency order, then each finding
confirmed by running something — turned up six more. What they have in common is that they
are all *cross-scope*: each one is a disagreement between two parts that are individually
correct and individually tested.

**A deactivation cascade could not hand its own binding back.** Unloading a provider retires
its bindings and reverts its effects only after the sweep, so a consumer that required them
still resolves them while it is being torn down — the ordering the first review fixed. But a
consumer that *also provides* something had its own binding removed the moment it was
deactivated, and the sweep only reached the plugin that required it on the next pass. So in
a chain of three, the far end's `unmount` resolved nothing. The fix is the same discipline
one level up: a deactivation retires and parks, the sweep runs to a fixed point with every
binding still resolvable, and only then is anything withdrawn — a withdrawal waits for the
deactivations it causes, at any depth. The suite could not see it because every fixture in
`nanus-kernel/tests/composition.rs` was a *pair*: no test had a plugin that both requires and
provides, so no test had a cascade. There is a three-plugin chain now, and it fails against
the old kernel.

**A tool result was paired with the wrong call.** A step writes every call it made and then
every result, so the entry before a result is the last call of the batch rather than the one
it answers. The recorded transcript therefore labelled a result with the *next* call's name —
which is not cosmetic: the interface pairs a result with the call above it, so a two-call
step drew the first call as still running, drew its output under the second tool, and drew
the last result twice. The log had the answer all along in `call_id`, which nothing used.
The replay now pairs by id, and the link was extended to carry
the id with each tool frame so the live view pairs by identity too. Both fall back to name and
then order for an entry that arrives without one — exact for a step whose calls name different
tools, and the best available for two same-named calls, which only an id can tell apart. Both
are tested against a two-call step, since a one-call step cannot tell either of them apart.

**`bash` ran in the wrong directory.** The tool's schema says its working directory defaults
to the workspace root, and the system prompt repeats it. It sent no working directory at all,
so the child inherited the *process's* — identical while the workspace root is unset, and
different the moment it is configured, which is also the whole of the difference for a
service. Every test ran with the two the same.

**The agent advertised a toolset it could not dispatch.** The runner was built over one
`ToolRegistry` and the plugin published a second, both built from the same ports. Nothing
looked wrong until something registered an eighth tool: the count in the handshake — what
`nanus service status` prints — went up, and the schemas on the next request did not.
`compose` now builds one registry and hands the same handle to the runner and to the
publisher, and the invariant is a `ptr_eq` postcondition in `Pending::start` rather than a
comment.

**Every tool discarded the correction it had just built.** `Arguments` exists so a malformed
call becomes a message the model can act on, and it says so in its own documentation — but
every caller read an optional field with `unwrap_or(None)`, so `{"limit": "ten"}` quietly
meant the default and the model learned nothing. The `finish` helper written for this was
dead code, used only by its own test. The reads propagate their failures now, and `finish` and
`result_of` are gone rather than left as a shape nobody adopted.

**Two dispatch modes were one function.** `Context::serial` and `Context::bail` shared a body,
so both stopped at the first decision while three doc comments promised that `serial` gives
every listener a turn. They differ now, and the test that claimed to compare them actually
calls both.

Three smaller ones, recorded because they are the same kind of thing: `Ctrl+C` at an approval
prompt did nothing (the turn is asleep on the answer, so the stop flag never reached its
checkpoint — the key now denies the call *and* asks for the stop), `--scroll` without
`--session` was accepted and ignored, and a capped search reported `truncated` whenever the
cap was *reached* rather than when a match was dropped, which is a confident falsehood a
model cannot check.

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
