# The interface

One binary, one harness. `nanus run` is the headless path you script, and `nanus tui` is
the interface you sit in front of. Neither is a wrapper around the other: both compose the
same plugin tree, load the same configuration, and drive the same agent loop, so a tool
that works in one works in the other.

![nanus tui showing a recorded conversation](images/tui-session.png)

*Real output, captured from the binary with `tmux capture-pane` — not a mock-up. The
conversation is a recorded session, which is why it can be shown without a key.*

## Starting it

There is nothing to enable: a plain release build has the interface in it.

```sh
cargo build --release

# Talk to a model in the current directory.
export DEEPSEEK_API_KEY=...
./target/release/nanus
```

A bare `nanus` **is** the interface when there is a terminal, which is the whole point of
one binary: the thing you type to get help and the thing you type to get a prompt are the
same word. `nanus tui` is the explicit spelling, for when you want to be sure or when the
default would be ambiguous.

It needs `DEEPSEEK_API_KEY`, because it composes a harness on start.

Without a terminal — piped, redirected, in a script — a bare `nanus` prints its usage
instead. It does not try to draw on something that is not a screen, and it does not fail:
nothing was asked for, and it answered.

### Or read a conversation you already had

The useful part: reading a transcript needs no credential at all, because it is already
written down. Every run persists its session, so the interface doubles as a browser for
them.

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
finished transcript would need a model this mode deliberately does not have.

A recording also opens with a header naming the session — title, directory, event count —
because a reader who was not there needs to know what they are looking at. A live
conversation gets no such header: the person is already in it.

## Keys

| Key | Effect |
|---|---|
| `Enter` | submit |
| `Alt+Enter` | newline |
| `Ctrl+W` | delete the previous word |
| `Ctrl+L` | clear the transcript |
| `Up` / `Down` | browse submitted prompts |
| `PageUp` / `PageDown` | scroll the transcript |
| `Left` / `Right`, `Home` / `End` | move the cursor |
| `Ctrl+C` / `Ctrl+D` / `Esc` | quit |

## What it shows, and why

**Reasoning is dimmed and italic; the answer is not.** They are different things, and a
reader scanning for the answer should be able to skip the thinking without reading it.

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

Raw-mode input and the alternate screen need a real terminal, so those are exercised by
hand rather than in CI. That is the honest limit, and it is why the logic lives in the view
layer instead of the loop.
