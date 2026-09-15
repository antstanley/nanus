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
agent loop already reports: text, reasoning, a step boundary, a tool starting and its
arguments, a tool finishing, usage, and the ending. Nothing an interface *might* want is in
it; anything else an interface needs about a conversation, the session log already holds.

The tool frame carries the arguments because a name is not enough to draw a call. `read`
says nothing a reader can use and `read` of one file says everything, and the half of the
turn that says *what the agent is doing* cannot be recovered anywhere else while a turn is
running: the session log has the arguments, but a client watching a turn is not reading the
log as it is written.

**A turn can be stopped by a client, because the turn is not the client's.** It runs in a
task the agent owns, in a session the agent holds, so that closing a terminal does not
abandon a turn — and the same design is why a client cannot simply *drop* one. The client
sends an `interrupt` request and the agent asks the turn to stop, which is the only party
that can: the agent is what holds the `&mut Session` the turn is writing. Nothing is sent
back, because a turn that stops ends with the ending frame it always ends with, and a client
that asked to stop a session which was not busy has asked for something already true.

**The ending says why the turn ended, not only that it did.** A turn can stop for reasons
that are not the model finishing — it can run out of steps, hit its token ceiling, be
interrupted, be refused by a policy, or fail — and one frame covers all of them because
the interface has to show what the model said *and* say what happened. It did not always:
the ending meant "the turn is over", so a turn that closed at its step budget arrived
looking exactly like a completed one, the last thing the model had said was drawn as its
conclusion, and the reader was left to work out from the silence that the work had been
cut off. `nanus run` had always called that a failed run and exited non-zero; the link is
where the same fact reaches a person watching, and it now carries it. The reason is the
link's own vocabulary rather than the domain's, because a bare client does not link the
domain — see the `server` feature in
[the manifest](../crates/nanus-link/Cargo.toml) — and the server's translation is an
exhaustive match, so a reason the domain grows cannot quietly fail to cross.

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

The bindings follow [Claude Code's interactive mode][cc-keys] where this interface has the
machinery to honour them, so that a reader arriving from there does not have to learn a
second set. Where it does not, the divergence is named rather than papered over.

| Key | Effect |
|---|---|
| `Enter` | submit |
| `\` + `Enter` | newline — the escape hatch that needs no terminal cooperation |
| `Alt+Enter` / `Shift+Enter` / `Ctrl+J` | newline |
| `Ctrl+C` / `Esc` | stop the running turn; then cancel the prompt; then quit |
| `Ctrl+D` | quit |
| `Ctrl+R` | reverse-search submitted prompts |
| `Ctrl+O` | switch between the one-line form and the whole of a tool call |
| `Ctrl+T` | summarise runs of tool calls |
| `Ctrl+E` | summarise runs of reasoning |
| `Ctrl+K` | delete to the end of the line |
| `Ctrl+U` | delete the line |
| `Ctrl+Y` | put back what `Ctrl+K` or `Ctrl+U` deleted |
| `Ctrl+W` | delete the previous word |
| `Alt+B` / `Alt+F` | move the cursor a word back / forward |
| `Ctrl+L` | clear the transcript |
| `Up` / `Down` | move between lines, then browse submitted prompts |
| `PageUp` / `PageDown` | scroll back and forward through the conversation |
| `Left` / `Right`, `Home` / `End` | move the cursor |

**`Ctrl+R` searches the history** rather than toggling anything, because that is what it is
in every interface that has one — including the one these bindings are modelled on, where
the same key does the same thing. The thinking summary moved to `Ctrl+E` to make room. The
search takes over the composer and the status line: what you type narrows the query rather
than editing the prompt, the composer shows the match, and the status line says
`(reverse-i-search)\`query'`. `Ctrl+R` again walks to older matches and stops at the oldest
one rather than emptying the screen; `Tab` or `Esc` takes the match and leaves it to be
edited; `Enter` takes it and sends it; `Ctrl+C` abandons the search and gives back whatever
was being typed. Matching ignores case, because a prompt is prose and a search that could
not see `Refactor` when asked for `refactor` reads as broken rather than strict.

**`Ctrl+C` and `Esc` stop what is happening, in the order a reader means it.** A turn in
flight is stopped first, because that is the thing happening now and the thing a reader
pressing "stop" is looking at. With nothing running the key reaches the prompt, and only an
empty prompt leaves — a key that means "stop" should not be able to lose a prompt somebody
is halfway through writing. `Esc` and `Ctrl+C` are the same key here for the same reason:
what a reader wants stopped is whatever is happening, and the key should not need reading
the screen first.

Stopping a turn is a *request*, not a keystroke. The turn belongs to the session rather
than to the terminal: it runs in a task the agent owns, so that closing a window does not
abandon it — and the same design means a window cannot end it either. The interface sends
`interrupt` and says `stopping` until the agent answers; the turn then closes with the
reason it always closes with, `interrupted`, and the transcript says so. The turn stops at
the next point where stopping is safe, which is between steps and between the tokens of a
model response — so what the model has already said is kept, while a tool call it was part
way through naming is dropped rather than recorded as one that ran. A tool that is *already
executing* finishes first: the tool contract has no way to cancel one, and a `bash` command
that would not stop is a process to kill rather than a turn to interrupt.

**`Ctrl+O` is the same choice `tui_detail` makes**, reachable without editing a file and
restarting, because which form a reader wants depends on what they are doing at that moment
rather than on how they started.

[cc-keys]: https://code.claude.com/docs/en/interactive-mode

### What is deliberately missing

Claude Code's mode has more bindings than this interface has things to bind them to, and
inventing a purpose for a key would be worse than leaving it alone:

- **Permission modes** (`Shift+Tab`) — approvals are the agent's policy, set in
  configuration, and this interface has no dialog to switch them from.
- **Model switching** (`Alt+P`) and **extended thinking** (`Alt+T`) are agent-side
  decisions with no request to carry them.
- **Background tasks** (`Ctrl+B`) — there are none to background.
- **Pasting an image** (`Ctrl+V`) would need clipboard access this program does not have.
- **`?` for a key list** is not implemented: the composer needs `?` to be a `?`, and
  swallowing it on an empty prompt is a cost this interface is not willing to pay for a
  list that is one `Ctrl+L` away from being off screen anyway. This table is that list.
- **Vim mode**, `@` mentions and `!` bash mode are input *modes* rather than shortcuts, and
  each is a feature in its own right. Slash commands have begun — see below.

### Commands

A line whose first word opens with `/` is a command, and the interface answers it rather
than sending it to the model.

| Command | Effect |
|---|---|
| `/exit` | leave the interface |
| `/quit` | the same command under its other name |
| `/stats` | write the session's model figures into the transcript |

`/stats` exists because the row under the composer cannot hold everything. Four readings fit
on a glanceable line and the session has more than four: the report adds the totals, the
prompt broken into cached and read, how much of what was generated was thinking, and prompt
tokens per second while waiting. It is a notice rather than prose — the model did not say it,
the interface did — and it reports the session rather than the last request, so it is worth
reading after a few turns and not before the first.

Nothing else is a command yet, and an unrecognised one is not sent to the model: it is
named in the transcript along with the commands that do exist, because a typo should say so
rather than spend tokens answering a question nobody asked. The cost of the convention is
that a prompt opening with a path — `/etc/hosts is wrong` — is read as a command attempt
and named as one. That is the trade every interface with this convention makes.

**Leaving takes the agent with it, unless the agent is a service.** `nanus tui` starts an
agent whose lifetime is the interface's: the core serves until the interface exits and then
shuts the composition down, which is what "scoped to the shell session" means. A bare
`nanus-tui` attached to a `nanus service` is a *client* of something with its own lifetime,
so leaving it stops nothing but the interface — which is the difference between closing a
window and stopping a server. A turn still running when the interface leaves is abandoned
rather than finished: the agent aborts it, and an aborted turn is not recorded.

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

A related hazard worth knowing before adding a binding of your own: **a terminal that
reports modifiers attaches `SHIFT` to the characters those modifiers produce.** `?` arrives
as `Char('?')` *with* `SHIFT`, not as a bare `?`. A binding that compares whole key events
therefore fails for exactly the keys a person tests by pressing them — `!`, `?`, `#` —
while `Shift-?` works, which is [ratatui/templates#26][shift-issue]. This interface reads
the key *code* and ignores the modifiers when inserting text, so typing is unaffected; a
new binding should do the same, and there is a test that fails if it does not.

[shift-issue]: https://github.com/ratatui/templates/issues/26

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

The caret is a **style on the cell it is over**, not a glyph in a cell of its own. Drawn as
a glyph it took a column of its own, so every character after it slid one place to the right
whenever the cursor moved — which reads as the cursor displacing the text it is moving
across, and is worst exactly where a caret is most useful: in the middle of a word being
corrected. Moving the cursor through `corvid` now leaves the word drawn identically at every
position, with only the reversed cell moving. Past the last character there is nothing to
reverse, so the caret becomes one cell: a block where the next keystroke will land.

A row that is exactly full has no column past its last character either, and the caret used
to have no cell to reverse anywhere — it vanished for the keystroke in which a prompt crossed
a row boundary. A terminal moves the cursor onto the next line there, and so does this: the
caret becomes the first cell of the following row, which is the continuation indent when
there is more text below, and a new row when the caret was already on the last one. That is
the cell the next character will occupy, because a character typed at the end of a full row
wraps onto a row of its own — so the block is drawn where the text will appear, not merely
somewhere visible.

The reversal is a *modifier*, which matters because of `NO_COLOR`. The backend sets a cell's
colours in one command covering foreground and background, and when crossterm is suppressing
colour that command degenerates to a bare `ESC[;m` — not "no colour" but a full SGR reset,
which clears the modifiers set for the same cell immediately before it. Under the colour
theme the caret was therefore invisible whenever it landed on a coloured prompt prefix, while
staying visible over the text beside it. A terminal that sets `NO_COLOR` (to anything
non-empty) now gets `Theme::monochrome`, which is the colour theme with the colours taken
out and the modifiers kept, so there is no colour command left to degenerate. The decision
belongs where the environment is read, in the runtime, rather than in the view: a view that
consulted the environment would render differently in whichever test inherited the variable.

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

## One line for the machinery, by default

The two parts of a turn that are *about* the work rather than the work itself are drawn as
one line each:

```text
── thinking · the glob is anchored to the wrong directory, so let me check the caller
✓ Read File · crates/nanus-bundle/src/tools/glob.rs
  <the file, first few lines and a count>
```

**A tool call is one line, naming the tool, what it is acting on, and how it went.** The
line is marked `⚙` while the call is running, `✓` when it finished and `✗` when it reported
a failure, so the outcome is on the call rather than on a second line under it. `✓ Read File
· crates/nanus-bundle/src/tools/glob.rs` says what `⚙ read({"file_path":…})` said, in words,
without the argument block. The argument the line reports is the one the tool acts on — the
file for `read`, `write`, `edit` and `read_image`, the pattern for `glob` and `grep`, the
command for `bash` — and a tool the interface has not been told about keeps its own name
rather than being shown under a guess. A multi-line command shows its first line and says
so. Nothing is invented: every field comes from the arguments the model sent, which is why
the link's `Tool` frame carries them.

**The outcome is on the call's line because there is nowhere else for it to go.** A live
transcript draws one line per call and no output — the frame that ends a call says only that
it is over and whether it failed, because a tool's output is in the session log — so the
call and its mark have to be the same row. A replayed transcript draws the same line and then
the output beneath it, summarised to its first few lines and a count.

**A thinking segment is one line, and it is the newest one.** As the model writes, the line
either grows or is replaced, so what a reader sees is the sentence the model is in the
middle of — and a cursor sits at its end while the segment is still arriving. Its earlier
paragraphs are not drawn at all; `Ctrl+E` is not a substitute for that, because it folds a
whole run into a count rather than showing the live end of it.

**Neither line wraps, and neither carries a blank row of its own.** A line too long for the
terminal is clipped from the front, keeping its label and the *end* of what it is acting on —
`✓ Read File · …tools/glob.rs` — because the newest words are the ones that say what is
happening now. And a tool's call sits directly against its result: no `── tool` heading
repeating what the line already said, and no row for the result's own name, which the call's
line has already carried along with its outcome. What still separates one piece of tool
activity from the next is the blank row every *other* entry is followed by — that row belongs
to the prose above it, not to the call below it.

**`Ctrl+O`, or `tui_detail = "full"`, is the way back.** The setting in the
[configuration file](../crates/nanus-adapter-config) — `compact` by default, `full` for the
whole argument block and the whole thinking segment — is read by the interface itself, from
the same file the core reads, so `nanus tui`, a bare `nanus-tui` against a service, and
`nanus tui --session` all draw the same transcript. The file holds the standing preference
and `Ctrl+O` toggles it for the session, because which of the two a reader wants depends on
what they are doing at that moment as much as on how they started; the two `Ctrl` summary
toggles remain the way to fold runs away entirely.

## What it shows, and why

**The answer is white; everything that is not the answer is marked.** Reasoning is dimmed
and italic, tool activity is yellow, notices are blue — so a reader scanning for the answer
can skip the thinking without reading it, and the thing they came for is not competing with
a colour of its own.

**Tool calls are paired with their results.** A call renders as `⚙ name · what it is doing`
and its result as `✓ name` or `✗ name`, so a failure is visible at a glance rather than
being buried in output.

**Long tool output is summarised with a count.** A `read` can return thousands of lines;
the transcript shows the first few and says how many were left, because the full text is
in the session log where it belongs.

**Recorded transcripts read exactly like live ones.** They are built from the same event
log by the same renderer — [the replay module](../crates/nanus-tui/src/replay.rs) folds
`SessionEvent`s into transcript entries and nothing invents content — so what you see
browsing is what you saw live.

**A turn that stopped early says so, in words.** When the ending is not a completion the
transcript gets a notice naming the reason — `the turn stopped at its step budget after 32
steps, so the work is unfinished`, or the model's token ceiling, or the failure — instead
of the last thing the model happened to say being offered as its conclusion. The sentence
is written by the interface rather than sent by the agent, because it is the interface's
job to phrase what a reader sees, and every reason gets its own phrasing rather than a
generic one: `max_steps` is a label, not an explanation.

**The answer is drawn once.** It arrives twice on purpose — streamed in deltas as it was
generated, and whole in the frame that ends the turn, so that a client which attached late
or lost a delta still ends up with it. The interface reconciles rather than appends: when
the streaming tail already holds exactly that text it is settled instead of being followed
by a second copy. Appending drew every completed turn's answer twice, once where it was
written and once after everything the turn did afterwards.

**The readings under the composer** are the model's, not the session's:

| | |
|---|---|
| `last 41/23 tok/s` | the last request's two rates: generating, then over its whole active time |
| `avg 150/140 tok/s` | the same pair over the session's totals |
| `ttft 0.8s` | how long the last request waited for its first token |
| `cache hit 96%` | the share of the session's prompt tokens the provider served from its cache |

**Every rate is written as a pair, and the pair is the point.** A request's active time is the
wait for its first token, the generation, and however long the stream took to close — and only
the middle of those is the model generating. So there are two honest answers to "how fast": the
first figure divides generated tokens by the *generation* alone, which is the model's speed,
and the second divides the same tokens by the *whole* request, which is the speed a reader
actually waited at. The gap between them is the wait, and in a coding session the wait
dominates: it is where the prompt is read, and a tool call is a short generation behind a long
one. Reporting only the first would flatter the model on exactly the steps a session is mostly
made of; reporting only the second would blame it for the prompt. Both are shown, so the reader
can see which they are getting — and `ttft` is beside them because it is the one figure anyone
can act on: the generating rate is the provider's and is not theirs to change, while the wait
is what a shorter prompt, or a cached one, buys back.

Wall-clock time is not what either rate is measured against, so a session that sat idle
overnight or spent five minutes inside a tool has the same averages as one that ran its
requests back to back. A rate that includes waiting measures the person waiting. Both count
*generated* tokens rather than the whole request: the prompt is mostly cache hits, so a
prompt-inclusive rate would mostly report how large the context had grown, and the cache share
is already the number for that side.

A request whose generation window the agent could not measure leaves the first figure of the
pair blank rather than restating the second under a different label; `avg` is over the requests
that reported a window, and only those, because a request that reported none would otherwise
contribute its tokens to the numerator and nothing to the denominator.

The line gives up its readings whole, from the end, before it gives up its row, and its row
before the composer gives up one of its own. The order is the order a reader would give them
up in: the rates first, then the wait that explains the gap between them, then the cache share
that usually explains the wait. A reading running off the end (`last 150/1`, with half of it
lopped off) is worse than one reading fewer, so a terminal too narrow for all four loses the
cache share rather than half a rate — and a terminal too short for everything loses the whole
row rather than the row the reader is typing on.

The average is over the session's totals rather than the mean of its per-request rates,
because a mean of rates is only an average when every request took the same time. A request
whose generation window the agent could not measure is left out of both sides of that average,
rather than contributing its tokens to the numerator and nothing to the denominator. The
arithmetic is whole numbers with `checked_*`, for the workspace's reasons, and a number that
has not been measured is drawn as a dash: zero is a measurement — it says the model generated
nothing — and showing it for "no request has finished yet" would be a claim rather than a
blank.

The link carries the request's time as a decomposition rather than as one figure — the wait, how
much of that wait went on reaching the server and being answered at all, the generation, the whole
request, and how much of the generation was thinking — because the parts answer different questions
and only one of them is the model's speed.

**The wait is split at the response head**, which is the one boundary this end of a socket can see.
Everything before it is connecting, uploading, and waiting to be answered at all; everything after
it is the server's own work — its queue, its reading of the prompt, and the first token. The split
is worth having because the two halves have different owners: a wait that is mostly the near half is
a network or an upload problem, and one that is mostly the far half is a prompt problem, and a
single duration cannot tell a reader which they have. `/stats` prints both halves.

It is also what makes the prompt-side figure worth reading. Prompt tokens per second is divided by
the server's *own* work rather than by the whole wait, and the time spent reaching the server cannot
contain prefill — prefill happens on the far side of the split — so charging that time to prefill
reported a rate lower than any the provider could have had. It is still a bound rather than a
measurement, and still a loose one: the server's work also covers its queue and the production of
the first token, and on a shared endpoint prefill is scheduled in chunks beside other requests, so
how long it takes is partly a property of the batch rather than of the prompt. Nothing at this end
separates those, and the report says `a bound, not a measurement` rather than implying an instrument
it does not have.

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
