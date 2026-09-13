# The interface

The interface is its own program. `nanus tui` starts an agent for the shell it was run
in and then runs `nanus-tui` against it; `nanus-tui` on its own connects to whatever
`nanus service` is running. Either way the interface is a client, and the agent can be a
process beside it or one that has been up since boot.

![nanus tui showing a recorded conversation](images/tui-session.png)

*Real output, captured from the binary with `tmux capture-pane` — not a mock-up. The
conversation is a recorded session, which is why it can be shown without a key.*

## Starting it

```sh
cargo build --release          # builds both binaries

# Talk to a model in the current directory.
export DEEPSEEK_API_KEY=...
./target/release/nanus
```

A bare `nanus` **is** `nanus tui` when there is a terminal. `nanus tui` and `nanus ui` are
the explicit spellings, for when you want to be sure or when the default would be
ambiguous.

That command composes an agent, binds a socket only your user can reach, and runs the
interface as a child process with the terminal inherited. When the interface exits, the
agent is torn down and the socket removed — so an agent started this way lives exactly as
long as the interface does, and a tool it started never outlives the screen it was
started from.

Without a terminal — piped, redirected, in a script — a bare `nanus` prints its usage
instead. It does not try to draw on something that is not a screen, and it does not fail:
nothing was asked for, and it answered.

## Where the agent is

| Command | The agent |
|---|---|
| `nanus` / `nanus tui` | Started by `nanus`, for this shell. Exits with the interface. |
| `nanus tui --connect` | Already running: whatever `nanus service` started. |
| `nanus-tui` (bare) | The same service socket as `--connect`. |
| `nanus-tui --link PATH` | Whatever is listening at `PATH`, which is how `nanus tui` hands one over. |
| `nanus tui --session` | None. A recording is a file, and reading it needs no agent. |

And which conversation it opens:

| Flag | What it opens |
|---|---|
| `--name X` | A new session recorded under `X`. |
| `--resume X` | An existing session, by name or id — the live one if the agent is holding it. |
| neither | A new, unnamed session. |

The session's name is shown in the title bar, because two terminals can be attached to two
different conversations and a reader should be able to tell which is which. See
[sessions](sessions.md) for what resuming and attaching mean.

The two binaries are installed together and the core looks for the interface *beside
itself*, never on `PATH`: a `PATH` lookup would happily run one version's interface
against another version's protocol. `NANUS_TUI` overrides the path for a build layout
neither can predict.

## The link

A Unix domain socket in `<nanus home>/run/`. One frame per line of JSON, both directions.

It is a socket rather than shared memory because **there is no safe in-process channel
between two processes**: sharing memory across a `fork` needs `mmap` and `unsafe`, and
this workspace forbids `unsafe` everywhere. The socket is the local equivalent — the
kernel copies bytes between two file descriptors and no packet reaches a network
interface. There is no port and nothing listening on an address, the run directory is
`0700` and the socket `0600`, and the reachable set is therefore "processes already
running as you", which can read the workspace and the session log anyway.

The protocol is deliberately tiny. A client says what it wants — start a session, attach
to one, list the ones the agent is holding, ask a question, send a prompt, stop — and the
agent answers with its handshake, the attachment, and then the same progress callbacks the
agent loop already reports: text, reasoning, a step boundary, a tool starting and
finishing, usage, and the ending. Nothing an interface *might* want is in it; anything
else an interface needs about a conversation, the session log already holds.

**A connection is a view of a session, not a session.** The agent owns the conversation,
so it survives the connection that opened it, and a client joins one with `--resume`.
Several clients can be attached at once and all see the same frames, which is what makes
watching a running conversation possible. One turn runs at a time, because a turn owns the
log — a prompt to a busy session is refused rather than queued. [Sessions](sessions.md) is
the whole of it.

**Nothing about the conversation travels twice.** The link carries what *happened*; the
history a client shows comes from the store, where it is already durable. A socket that
also carried the log would be a second source of truth for something that has one.

**The session is recorded before the ending is sent.** A client that has seen the answer
is holding one whose transcript is already on disk, which is the same contract `nanus run`
keeps with its own stdout.

## Or read a conversation you already had

Reading a transcript needs no credential at all, because it is already written down.
Every run persists its session, so the interface doubles as a browser for them.

```sh
nanus sessions                    # list what is available
nanus tui --session               # read the most recent one
nanus tui --session <id>          # read a particular one
nanus tui --session --scroll 50   # open fifty rows back from the end
```

`--scroll` matters more than it sounds. A conversation opens at its end, where the answer
is; the middle is where the reasoning and the tool calls are, and that is usually what you
want to look at when asking *why* the agent did something. The count is in display rows,
which is what the viewport is measured in — a line that wraps occupies several rows, so a
count in lines would put the end of a long conversation out of reach.

In a recorded session the composer still works, and submitting tells you to start `nanus`
without `--session` rather than silently discarding what you typed. Adding a turn to a
finished transcript would need an agent this mode deliberately does not have.

A recording also opens with a header naming the session — title, directory, event count —
because a reader who was not there needs to know what they are looking at. A live
conversation gets no such header: the person is already in it.

## Keys

| Key | Effect |
|---|---|
| `Enter` | submit |
| `Alt+Enter` / `Shift+Enter` / `Ctrl+J` | newline |
| `Ctrl+W` | delete the previous word |
| `Ctrl+T` | summarise runs of tool calls |
| `Ctrl+R` | summarise runs of reasoning |
| `Ctrl+L` | clear the transcript |
| `Up` / `Down` | move between lines, then browse submitted prompts |
| `PageUp` / `PageDown` | scroll back and forward through the conversation |
| `Left` / `Right`, `Home` / `End` | move the cursor |
| `Ctrl+C` / `Ctrl+D` / `Esc` | quit |

`Shift+Enter` needs a word, because how it reaches a program is not what you would expect.
**It is not one key.** Two different things can happen when you press it:

- **The terminal types a character.** A line feed, `0x0A`. This is what
  [Ghostty](https://ghostty.org) does by default: `shift+enter` is bound to "send a
  newline" rather than reported as a key, so nothing about key protocols is involved — the
  terminal is typing at you. In raw mode a line feed *is* `Ctrl+J`, so the interface treats
  `Ctrl+J` as a newline. That is what it has meant since readline, so it is one binding
  rather than a special case.
- **The terminal reports a key**, if it speaks the
  [kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/) and the
  interface asks for it. The interface asks when the terminal answers that it speaks it,
  and does not ask when it does not — a terminal that does not understand the request may
  print the escape sequence instead. Then `Shift+Enter` arrives as itself.

`Alt+Enter` is the spelling that works regardless, which is why it is documented first.

## Scrolling back

The conversation follows the newest output until you scroll away from it, and follows
again when you scroll back to the bottom. There is no key to press to resume and none to
remember: `PageUp` means "I am reading something", and coming back down means "carry on".

That rule exists because the alternative is unusable. Every streamed token used to pull the
view to the bottom, so a reader who scrolled up during a turn was dragged back down on the
next one — and from the bottom of a live conversation, where the view always sits, `PageUp`
was also adding to an offset that counts rows skipped from the *top*, so it clamped and
appeared to do nothing at all. Both are fixed, and both directions are pinned by tests.

## The composer

`Enter` sends and `Alt+Enter` — or `Shift+Enter`, where the terminal reports it — starts a
new line, so a prompt can be a paragraph rather than a sentence. `Up` and `Down` move
between those lines, and only from the top line do they browse submitted prompts, which is
what keeps the single-line case behaving exactly as it did.

The composer grows with the prompt up to five rows and then scrolls to keep the line being
typed on screen, so a long prompt stays editable without squeezing the conversation out
of the terminal.

Its wrapping is done here rather than left to the drawing library, and that is deliberate.
The row a character lands on is what decides whether the composer has to scroll, and a
word-boundary wrapper moves a word that does not fit onto a new row instead of filling
the one before it. Counting characters cannot see that, so the caret came out one row
below the window on exactly the prompts long enough to need the scroll. Owning the wrap
makes the caret's row a fact the interface knows rather than an estimate it hopes is
right.

## Summarising what is not the answer

Two toggles fold the parts of a turn that are *about* the work rather than the work itself:

| Key | Folds |
|---|---|
| `Ctrl+T` | runs of consecutive tool calls into `── 2 tool calls · bash, read` |
| `Ctrl+R` | runs of reasoning into `── thinking · 1 part · 822 characters` |

Two things make this useful rather than lossy. **A run is summarised, not each entry**: six
tool calls in a row are one thought the model had, and six collapsed lines would be as noisy
as the six lines they replaced — while two tool calls with an answer between them are two
runs, because they are two thoughts. And **the summary keeps what a reader scanning for a
problem needs**: how many calls, which tools, and whether any failed. The status line names
what is folded, so a toggle is never a mystery about why the transcript looks short.

## What it shows, and why

**The answer is white; everything that is not the answer is marked.** Reasoning is dimmed
and italic, tool activity is yellow, notices are blue — so a reader scanning for the answer
can skip the thinking without reading it, and the thing they came for is not competing with
a colour of its own.

**Tool calls are paired with their results.** A call renders as `⚙ name(args)` and its
result as `✓ name` or `✗ name`, so a failure is visible at a glance rather than being
buried in output.

**Long tool output is summarised with a count.** A `read` can return thousands of lines;
the transcript shows the first few and says how many were left, because the full text is
in the session log where it belongs.

**Recorded transcripts read exactly like live ones.** They are built from the same event
log by the same renderer — [the replay module](../crates/nanus-tui/src/replay.rs) folds
`SessionEvent`s into transcript entries and nothing invents content — so what you see
browsing is what you saw live.

## Why the interface is testable

The view is a pure function of a `Transcript` and an `InputBuffer`, neither of which knows
what an agent is. That is what lets scrolling, wrapping, key handling, and role rendering be
asserted against ratatui's `TestBackend` in a headless test — including that each role's
colour actually reaches the rendered cells, which is not something reading the theme would
reveal.

The link is tested the same way, with a real socket in a temporary directory and a scripted
model on the other end: a prompt, a streamed answer, a session on disk, a status reply, a
shutdown request, and the negatives — a socket nobody is listening on, a stale socket file,
a peer that says something that is not a frame. What no automated test can do is *take a
terminal*, so raw-mode input and the alternate screen are exercised by hand. That is the
honest limit, and it is why the logic lives in the view layer instead of the loop.
