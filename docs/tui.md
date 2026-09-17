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

`--approval` takes `per_call`, `permitted`, or `all_calls` and is the state the interface
opens in — and, for a live conversation, the state it asks the agent to use. The status
line always names the state in force, and `Shift+Tab` opens the dialog that chooses it.

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
to one, list the ones the agent is holding, ask a question, send a prompt, stop, answer an
approval question — and the agent answers with its handshake, the attachment, and then the
same progress callbacks the agent loop already reports: text, reasoning, a step boundary, a
tool starting and its arguments, a tool finishing, usage, and the ending. Nothing an
interface *might* want is in it; anything else an interface needs about a conversation, the
session log already holds.

The tool frame carries the arguments because a name is not enough to draw a call. `read`
says nothing a reader can use and `read` of one file says everything, and the half of the
turn that says *what the agent is doing* cannot be recovered anywhere else while a turn is
running: the session log has the arguments, but a client watching a turn is not reading the
log as it is written.

Both tool frames also carry the call's **id**, which is what pairs a call with its result.
Position cannot: a step sends every call it made and only then every result, and a step's
results go out in the order the tools finished rather than the order they were asked for —
so two calls to one tool in one step are indistinguishable on screen without it, and the
transcript would show the wrong one as having failed. The field is optional, and a client
reading a frame without one falls back to pairing by name and then by order, which is exact
for a step whose calls all name different tools. Adding it changed no frame's meaning, so
the protocol version stands.

**A turn can be stopped by a client, because the turn is not the client's.** It runs in a
task the agent owns, in a session the agent holds, so that closing a terminal does not
abandon a turn — and the same design is why a client cannot simply *drop* one. The client
sends an `interrupt` request and the agent asks the turn to stop, which is the only party
that can: the agent is what holds the `&mut Session` the turn is writing. Nothing is sent
back, because a turn that stops ends with the ending frame it always ends with, and a client
that asked to stop a session which was not busy has asked for something already true.

**The handshake carries a protocol version.** The two binaries ship together — `nanus tui`
runs the interface from beside the core and never looks on `PATH` — but nothing stops a
stale `nanus-tui` from sitting next to a rebuilt `nanus`, and without a version the first
frame whose shape changed is a decode error naming a field halfway through a turn. The
handshake says which version the agent speaks, a client refuses anything else with a
sentence naming both, and a build too old to send a version reads as version zero — refused
rather than assumed compatible. `nanus service status` prints the version the running agent
reported.

**A tool call outside the sandbox is decided by whoever is watching.** When the loop
reaches a call the sandbox does not already permit and the state does not grant it — `per
call` grants nothing, and `permitted calls` grants the non-destructive ones — the agent
sends an `approval` frame to every client attached to the session and the turn waits. The
frame carries the tool and the harness's own reason and deliberately *not* the call's
arguments: the domain's approval request carries none, so model-controlled text cannot be
placed in front of the person deciding, and the transcript already shows what the call is.
A client answers with an `approve` request naming the question; the first answer wins, and
denying is the safe reading of everything else. An answer may be standing: the "always
allow" option records the tool for the session, so the same question is not asked again.
Nobody attached, or a last client that detaches while a question is open, is an unavailable
answerer — the call is denied rather than left waiting for a decision that cannot arrive.
`all calls` never reaches the link at all: it grants every exception without asking.

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
log — a prompt to a busy session is refused rather than queued. That is the agent's rule,
and the interface answers it without making the reader wait: a prompt typed during a turn
is held and sent when the agent is ready, which is [queuing a prompt](#queuing-a-prompt).
[Sessions](sessions.md) is the whole of it.

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
| `y` | while an approval dialog is up: allow the call once |
| `a` | while an approval dialog is up: allow the call and record the tool for the session |
| `n` (or `Esc`) | while an approval dialog is up: deny it |
| `Ctrl+C` | while an approval dialog is up: deny it *and* stop the turn |
| `Shift+Tab` | choose the approval state, from anywhere |
| `Enter` | submit |
| `\` + `Enter` | newline — the escape hatch that needs no terminal cooperation |
| `Alt+Enter` / `Shift+Enter` / `Ctrl+J` | newline |
| `Ctrl+C` / `Esc` | stop the running turn; then cancel the prompt; then quit |
| `Ctrl+D` | quit |
| `Ctrl+R` | reverse-search submitted prompts |
| `Ctrl+Q` | open the queue of prompts waiting for the turn to end (and close it) |
| `Ctrl+O` | switch between the one-line form and the whole of a tool call |
| `Ctrl+T` | summarise runs of tool calls |
| `Ctrl+E` | summarise runs of reasoning |
| `Ctrl+K` | delete to the end of the line |
| `Ctrl+U` | delete the line |
| `Ctrl+Y` | put back what `Ctrl+K` or `Ctrl+U` deleted |
| `Ctrl+W` | delete the previous word |
| `Alt+B` / `Alt+F` | move the cursor a word back / forward |
| `Ctrl+L` | clear the transcript |
| `?` | show the key list, when the prompt is empty |
| `Alt+P` | switch to the next model the agent offers |
| `Alt+T` | ask for the next step of reasoning effort |
| `Ctrl+V` | paste an image from the clipboard, as a path |
| `@` | name a file; the menu completes it with `Tab` |
| `!command` | run a shell command here, without the model |
| `Up` / `Down` | move between lines, then browse submitted prompts |
| `PageUp` / `PageDown` | scroll back and forward through the conversation |
| `Left` / `Right`, `Home` / `End` | move the cursor |

**`?` opens the key list**, which is this table drawn on the screen: a reader who does not
know a binding exists has no way to find it, and documentation they are not looking at is
not a list. The gate is the composer: `?` opens the list only when there is nothing being
typed, because a prompt needs `?` to be a `?` — `why?` is a question, not a command. Nothing
being typed means nothing but whitespace, so a prompt holding only spaces still gives the
key to the list. The overlay owns the keyboard while it is up — `Esc`, `Enter`, `Ctrl+C`,
`q`, or `?` again closes it, `Up` and `Down` scroll it, and every other key does nothing
rather than typing into the composer behind it — and the list scrolls rather than being cut,
because a binding that fell off the bottom of a short terminal would be exactly the one a
reader opened the list to find.

**While an approval dialog is up, four answers are possible and every other key is
swallowed.** The dialog is drawn over the interface, names the tool and the harness's reason,
and names every option with the key that selects it. `y` allows that one call, `a` allows it
and records the tool for the rest of the session so the question is not asked again, `n` or
`Esc` denies it, and `Ctrl+C` denies it *and* asks the turn to stop — because the turn is
asleep on this answer, so the key that means "stop everything" everywhere else would
otherwise do nothing at all here. A stray keypress cannot approve a command. The status line
says what is being waited for, and the dialog closes when the turn ends — an answer cannot
outlive the question.

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

### Queuing a prompt

A turn owns the session's log, so the agent serves one turn at a time and refuses a second
prompt rather than interleaving it. The interface turns that refusal into a queue: a prompt
typed while a turn is running is held and sent as soon as the agent is ready for it — one
per turn end, in the order it was typed. Typing ahead is therefore ordinary rather than
refused, and the agent is still asked for one turn at a time exactly as a person would be.

The same holds when the session is already busy as the interface opens — attaching to a
turn another client started — because a client that joins late has missed the prompt that
began it. The status line says a turn is already running, and the reader's first prompt
queues behind it rather than being sent to an agent that cannot take it.

Queued prompts are listed above the composer, oldest first, because the oldest is the one
that runs next: the list is a schedule rather than a history. The status line counts them,
and the list is bounded — past three entries the rest are counted — so a queue can never
push the conversation off the screen. A queued prompt is *not* in the transcript yet: it
has not been sent and the model has not seen it, so it appears as an ordinary prompt only
when the turn before it ends. The queue is the interface's and not the session's, so it is
deliberately not written into the log: it dies with the terminal that typed it rather than
being replayed to the next client that attaches.

`Ctrl+Q` opens the overlay, which is where a queue is read and changed, and it owns the
keyboard while it is up:

| Key | Effect |
|---|---|
| `Up` / `Down` (or `k` / `j`) | move the selection |
| `Enter` (or `e`) | pull the selected prompt into the composer to edit it |
| `d` / `Delete` | remove the selected prompt |
| `Esc` (or `q`, `Ctrl+Q` again, `Ctrl+C`) | close the overlay |

**Editing takes the prompt out of the queue and into the composer**, so a turn that ends
mid-edit cannot send the half-read text. `Enter` saves it back at the position it came
from; `Esc` or `Ctrl+C` cancels and gives back both the original prompt and whatever draft
was in the composer before the edit began. Deleting every character and saving removes the
entry, because an empty prompt is not one worth sending. The one place this differs from
every other mode is `Enter`: everywhere else it sends, and here it keeps the edit — which
is what the status line says while the composer is holding a queued prompt.

### Mouse

The mouse navigates as well as the keyboard. The wheel scrolls the conversation three rows
a notch, in the same direction and with the same follow rule as `PageUp`/`PageDown`, and a
left click in the composer puts the caret on the character it landed on, so a long prompt
can be corrected without arrow keys. A click anywhere else is not a command: the transcript
is read, not pointed at, and nothing in it is a target.

The wheel is taken wherever the pointer is rather than only over the conversation. A reader
reaching for it without looking should not have to find a band first, and the conversation
is the only thing here there is to scroll. Mouse reporting is asked for when the interface
starts and given back when it leaves, and while it is on the terminal's own
click-and-drag selection needs whatever modifier that terminal uses for it — usually
`Shift`. That is the cost of a program that draws its own screen, and it is the cost every
full-screen terminal program pays.

### What is deliberately missing

Claude Code's mode has more bindings than this interface has things to bind them to, and
inventing a purpose for a key would be worse than leaving it alone:

- **The sandbox mode** is set in configuration rather than from the keyboard: `Shift+Tab`
  chooses the approval state, but `sandbox_mode` is a standing decision about what the tools
  may touch, and changing it mid-turn would make the prompt the model was sent a lie. The
  dialog says so where a reader is choosing, rather than leaving the difference to be found
  in a document.
- **Background tasks** (`Ctrl+B`) — there are none to background. A `!` command is not one: it
  runs as a task so the interface keeps drawing, but it is not a job this interface can list,
  wait on, or kill, and inventing a list for one command at a time would be a task model rather
  than a key.

### Commands

A line whose first word opens with `/` is a command, and the interface answers it rather
than sending it to the model.

| Command | Effect |
|---|---|
| `/exit` | leave the interface |
| `/quit` | the same command under its other name |
| `/stats` | write the session's model figures into the transcript |
| `/help` | draw the key list, the same one `?` opens |
| `/clear` | empty the transcript, leaving the draft and the toggles alone |
| `/model [id]` | switch to the next model the agent offers, or to the one named |

`/stats` exists because the row under the composer cannot hold everything. Four readings fit
on a glanceable line and the session has more than four: the report adds the totals, the
prompt broken into cached and read, how much of what was generated was thinking, and prompt
tokens per second while waiting. It is a notice rather than prose — the model did not say it,
the interface did — and it reports the session rather than the last request, so it is worth
reading after a few turns and not before the first.

`/model` is the command form of `Alt+P`, and the pair is deliberate: the key cycles and the
argument names one. See [switching models](#switching-models).

`/help` and `/clear` are the screen's business rather than the session's, which is why they
are answered in a recorded session too: neither needs an agent, and a reader browsing a
transcript still has a keyboard. `/help` opens the same overlay `?` does rather than a second
list that says almost the same thing. `/clear` empties the transcript and nothing else — the
draft in the composer and the two summary toggles are about what the reader is doing now, and
`Ctrl+L` already means exactly this.

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

**The session is summarised on the way out.** Leaving prints a table of what the session did
and what it spent, on the screen the shell carries on from — the terminal is already back to
its normal mode and off the alternate screen by the time it is written, so the figures stay
where a reader can scroll back to them rather than disappearing with the interface.

It is two tables because the figures have two scopes. The first is the session's: the model
and the permission state it ran under, the turns, steps and requests, the prompt split into
cached and read, the generated tokens and how many of them were thinking, a row per model
when a session used more than one, and why the last turn ended. All of it is read from the
log, so it covers the whole conversation — a session that was resumed includes the turns that
ran before this interface existed.

The second is the run's: the same rates and waits the row under the composer reports, as
`last` and `average` side by side. Only the process that watched the responses arrive can know
them, which is why they are not in the log beside the totals — and why the table is absent
entirely when the interface watched nothing. Reading a recording with `--session` prints the
first table and no second one, because nothing was measured; a table of dashes would say
something had been measured and came back empty.

A reading nobody took is a dash rather than a zero, and a session recorded before the
configuration was written down says so once rather than showing five dashed rows.

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

### Choosing the permission state

`Shift+Tab` opens a dialog listing the three approval states with what each one means. The key
works from anywhere — including from behind an approval question, because a reader who wants to
stop being asked must not have to answer a question first — and it opens on the state the old
binding moved to, so `Shift+Tab` then `Enter` is the cycle it always was, with the chance to read
what it grants before granting it. That reading is the whole point: `permitted calls` and `all
calls` are two words apart and nothing alike in what they allow, and a status line can only name
one of them at a time.

Inside the dialog, `Up`/`Down` (or `k`/`j`) move, `Tab` and `Shift+Tab` walk forward, the digits
`1`–`3` select directly, `Enter` applies and `Esc` cancels. Every other key does nothing rather
than typing into the composer behind the dialog.

**The dialog moves one of the two knobs a preset bundles.** The approval state is what happens to
a call the sandbox refused, and it is the agent's to change mid-session. The sandbox mode — what
the tools may touch — is not: it is a standing decision the model was told about in the prompt it
was sent, and the dialog says so rather than offering a switch that would make that sentence a
lie. `PermissionPreset` and its three names are in
[design](design.md#approval-is-a-three-state-axis-fail-closed-at-the-default).

### Switching models

A model is chosen by configuration and can be changed without restarting: `Alt+P` moves to the
next one the agent offers and `/model <id>` names one, and the title bar says which model is
answering. The list comes from the agent's handshake rather than from the interface, because
which models exist is a decision of the composition — an interface that cycled a list of its own
would offer models the agent refuses — and the agent refuses an id it does not offer with a
sentence naming the ones it does. A session that offers nothing to switch to, which is every
recording, says so rather than drawing a list nobody would honour.

**The switch is the agent's, and it reaches every client.** The model belongs to the agent's
runner rather than to a session, exactly as the approval state does: one runner serves every
conversation it holds, and a client that switches tells every watcher, so two terminals attached
to one session cannot disagree about which model is answering. It takes effect on the next
request, including the next step of a turn that is already running, because what a reader
switching mid-turn is saying is what they want the *next* step to be.

**The system prompt names the model the composition started with**, exactly as it names the
startup approval state, and a switch does not rewrite it. The model does not need to be told its
own name, and the alternative — rebuilding the prompt on every switch — would mean the
conversation the model is being sent no longer matches the one recorded against it. The live
value is the one in the title bar; the recorded one is what the session says produced it, and the
two are the same thing until somebody switches.

### Running a command yourself with `!`

A prompt that opens with `!` is a shell command rather than a prompt. The interface echoes it,
runs it, and draws its output in the transcript as a notice, and the composer's mark changes from
`›` to `$` while the draft is one — so which `Enter` you are about to press is visible before you
press it.

**It is your command, not the agent's.** It runs with your environment and your privileges in the
directory the interface was started in, it is not sandboxed, it is not approved (there is nobody to
ask), and *nothing about it is recorded*: the model is never shown it, and the session log — the
model's history — does not have it. A command you want the model to see, or want confined to the
workspace, is one you should ask it to run: that is the `bash` tool, and it goes through the gate.
[SAFETY.md](../SAFETY.md#your-own-commands-the--escape) is where that argument is written out in
full, because it is a safety decision rather than a feature.

The command runs as a task rather than in the interface's own step, so a slow command does not stop
the agent's frames being read — a turn that is running keeps arriving while `!find .` works. Its
output is bounded to a few dozen lines and a few thousand characters, with a count of what was left
and a `[exit N]` line when the exit status was not zero.

### Naming a file with `@`

`@` starts a mention: a word in the prompt that names a file, completed from the workspace. Type
`@ma`, and a menu appears over the composer listing the files that answer it — `src/main.rs` before
`docs/maintenance.md`, because the file's own name counts for more than the directory it is in.
`Tab` completes the selected one, `Up`/`Down` choose, `Esc` closes the menu and leaves the word
alone, and typing another character narrows the list. Every other key still belongs to the
composer: a mention is a word being typed, not a mode, so `Enter` sends the sentence it is part of
rather than accepting the completion.

The menu is drawn above the composer and yields its rows to it on a short terminal, like the
queue. It lists at most six files and offers at most thirty-two, because this runs on a keystroke:
the walk of the workspace is bounded, it skips `.git`, `target`, and `node_modules`, and it is
remade whenever a mention *starts* rather than kept for the session — which is what offers a file
the agent wrote a minute ago. It does not read `.gitignore`: a file a reader has deliberately
ignored is still a file they may want to name.

**A mention expands to the path, not the file.** Inlining contents would put a file the model never
asked for into the request, spend the context on it, and make the prompt a thing the reader cannot
see all of. The model already has `read`; a mention is how it is told *which* file to read, and the
path is the whole of what is inserted.

### Pasting an image

`Ctrl+V` reads an image off the clipboard, writes it into the workspace under `.nanus/pasted/`,
and puts the path in the composer. Press Enter and the model is shown the path; it calls
`read_image` on it like any other file.

It is a path rather than bytes because that is what the wire has. `read_image` is how an image
reaches a model — a tool result with a content block — and the link has no request that carries
bytes a client produced; adding one would mean a frame whose payload is arbitrary binary. So the
file lands in the reader's own directory, where the tools can reach it, and it stays there: a
paste is not cleaned up, and a reader who does not want it keeps it or deletes it. A temporary
directory would not do, because the tools are rooted at the workspace and a path outside it is
refused.

The clipboard is read by the platform's own tool — `pbpaste` on macOS, `wl-paste` or `xclip`
elsewhere — because a terminal has no clipboard API and the crates that provide one pull in a
windowing system this program never opens. What comes back is checked by its magic number rather
than trusted: a reader that answered with the clipboard's *text* has not provided an image, and the
path inserted is never a `.png` full of prose. Nothing to paste says so on the status line, in the
three cases that are all the same to a reader: no reader installed, nothing on the clipboard, or
something on it that is not an image.

**A terminal may claim `Ctrl+V` for itself**, and most do: one that handles paste never sends the
key, and pastes *text* into the composer instead, which is what that key does everywhere else. This
binding is for the terminals that pass it through, and there is no way to ask which kind you have
other than to press it.

### How hard the model is asked to think

`Alt+T` steps the reasoning effort, and the title bar names the step in force. The scale is the
provider's own, four steps from `minimal` — which is how a provider says "do not think" — through
`low` and `medium` to `high`, and the key cycles it rather than toggling it: only one of the four
means "no thinking", so a toggle would have to invent what "on" means for a reader who had already
chosen `low`. A session that has not said which effort it is using starts above the middle, since a
reader pressing a key called extended thinking means more thinking rather than the setting they
already had.

Like the model, the effort belongs to the agent's runner and reaches every watcher, and it takes
effect on the next request rather than the next turn. An adapter with no notion of effort says so
by the absence: nothing is drawn beside the model, and a change is a request the adapter ignores
rather than an error.

## Scrolling back

The conversation follows the newest output until you scroll away from it, and follows
again when you scroll back to the bottom. There is no key to press to resume and none to
remember: `PageUp` — or the wheel — means "I am reading something", and coming back down
means "carry on".

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

What used to be a box is now a prompt between two rules. The sides and the `message` title
were more frame than a line of prose needs, and the rules still separate it from the
transcript above and the figures below. It is inset from the terminal's edges so the prompt
does not sit against them, and on a terminal too short for that padding the blank rows are
the first thing given up — the row being typed on is the last.

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

## The answer is markdown

The model writes markdown, so the interface parses its answer and draws the *rendered*
form: headings are headings rather than `#` lines, emphasis and inline code carry their
style, lists get bullets, and a fenced block is drawn as code. The source scaffolding is
never shown, because it was syntax rather than content.

What is supported is the subset a coding assistant actually emits: ATX headings,
paragraphs with word wrapping, `**bold**` / `*italic*` / `` `inline code` ``, links
(`[label](url)` becomes the label and its destination), fenced code blocks, ordered,
unordered, and task lists, nested blockquotes, horizontal rules, pipe tables, and
whole-line images. A leading `+++`-delimited TOML frontmatter block is stripped. A
construct that never closes — a `**` mid-stream, a fence still open — is drawn as itself,
so a half-arrived answer is readable rather than mangled.

**Only the model's answer is parsed.** Reasoning is still the newest line of itself and
tool output is still drawn verbatim, and that is deliberate: a `read` that returned a
unified diff or a file of `#` comments would otherwise turn into a bulleted list and a
wall of headings. The rule is the role, not the content.

**Mermaid is drawn as text.** A `mermaid` fence is parsed and rendered as a diagram:
flowcharts, sequence diagrams, pie charts, gantt charts, state diagrams, class diagrams,
quadrant charts, and block diagrams. A diagram that cannot be parsed — including one that
is still being streamed — falls back to showing the fence's source, because losing a
diagram is worse than showing it unfinished. Diagrams use the interface's own accents and,
like everything else, obey the column budget, so the scroll arithmetic stays exact.

Two settings turn the rendering off, both defaulting to on, in the same configuration file:

```toml
markdown = true   # render the model's answers as markdown
mermaid = true    # draw mermaid fences as diagrams
```

`markdown = false` draws every answer exactly as it arrived, and `mermaid = false` shows
the fence as code even when it would parse.

**The renderer does no I/O.** An image is a labelled placeholder, not a file read and not
a URL fetch: the view is a pure function of the transcript and the composer, and a remote
fetch on a model's say-so is a request the reader did not ask for. Control characters are
stripped at the parse boundary, so a model — or a file it read — cannot smuggle a terminal
escape sequence into the screen through a heading or a code fence. A fenced block *is*
highlighted, by a small lexer that lives beside the renderer and reads nothing: Rust,
Python, JavaScript and TypeScript, JSON, TOML, shell, and YAML are recognised, and a
construct that spans lines — a block comment, a docstring — stays itself across them. A
fence whose language the lexer does not know, and every fence when `markdown = false`, is
drawn verbatim in the code style, which is what all of them were before. The classes borrow
the interface's existing role colours — a keyword takes the tool accent, a comment the
reasoning style — so a monochrome terminal keeps the modifiers and loses only the colours.

The markdown styles are derived from the interface's role styles, so the answer keeps the
answer's colour and `NO_COLOR` works without a second palette: `Theme::monochrome` is the
colour theme with the colours removed, and the markdown theme inherits that.

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
