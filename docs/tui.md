# The interface

Two front ends, one harness. The headless CLI is what you script, and the TUI is what you
sit in front of. Neither is a wrapper around the other: both compose the same plugin tree
and drive the same agent loop, so a tool that works in one works in the other.

![nanus-tui showing a recorded conversation](images/tui-session.png)

*Real output, captured from the binary with `tmux capture-pane` — not a mock-up. The
conversation is a recorded session, which is why it can be shown without a key.*

## Starting the TUI

The TUI is built behind a feature, so it is one flag away from a plain `cargo build`:

```sh
cargo build --release -p nanus-tui --features runtime

# Talk to a model in the current directory.
./target/release/nanus-tui
```

It needs `DEEPSEEK_API_KEY`, because it composes a harness on start.

```sh
export DEEPSEEK_API_KEY=...
./target/release/nanus-tui
```

### Or read a conversation you already had

The useful part: reading a transcript needs no credential at all, because it is already
written down. Every run persists its session, so the TUI doubles as a browser for them.

```sh
nanus-tui --sessions              # list what is available
nanus-tui --session               # read the most recent one
nanus-tui --session <id>          # read a particular one
nanus-tui --session --scroll 50   # open fifty rows back from the end
```

`--scroll` matters more than it sounds. A conversation opens at its end, where the answer
is; the middle is where the reasoning and the tool calls are, and that is usually what you
want to look at when asking *why* the agent did something.

In a recorded session the composer still works, and submitting tells you to start
`nanus-tui` without `--session` rather than silently discarding what you typed. Adding a
turn to a finished transcript would need a model this mode deliberately does not have.

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
