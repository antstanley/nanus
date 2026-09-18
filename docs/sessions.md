# Sessions

A session is the conversation. It is written down as it happens, it can be given a name,
and it can be picked up again — by a later command, by another terminal, or by a client
that attaches to an agent that is still in the middle of it.

```sh
nanus run --name nightly "summarise what changed today"
nanus sessions                                  # list, with names
nanus tui --resume nightly                      # continue it
nanus sessions name project-x 01a09a98…         # or rename it later
```

## What a session is

An append-only log of events — turns, messages, tool calls and their results — plus the
identity of the conversation: a store key, a creation time, the directory it ran in, and the
harness configuration it ran under. It is the only copy. Everything a client shows is derived
from it, which is why the agent records before it answers and why a transcript is
reproducible rather than reconstructed.

```text
<nanus home>/
  sessions/
    01a09a98-d8c4-73d7-b11d-638077efeeca/
      session.jsonl      the conversation
      name               "nightly"           (optional)
      lock               the writer's pid    (optional, while held for writing)
```

The key is a time-ordered uuid, so a directory listing sorts by creation. The name is a
separate file beside the log, and that is deliberate:

- **A name is an alias, not identity.** The domain says a session id is a store key and
  the store decides what a key looks like. A human-typable second key is the same kind of
  decision, so naming a session never rewrites it and renaming keeps its identity.
- **No shared table.** A file per session means two writers cannot lose each other's
  aliases, and deleting a session takes its name with it rather than leaving an alias
  pointing at nothing.
- **A name is content, never a path.** It lives inside the session's own directory under a
  fixed file name, so no name can climb out of the store.

## Naming

A name is how a session is found again, so it is taken for good: starting a second session
with a name that is already held is refused, not moved. Silently reassigning an alias would
make `--resume nightly` open somebody else's conversation.

```console
$ nanus run --name nightly "…"
$ nanus run --name nightly "…"
nanus: the name "nightly" already belongs to session 01a09a98-d8c4-73d7-b11d-638077efeeca
```

The name is claimed *before* the turn runs, so a refused name costs a sentence rather than
a turn, and leaves no unnamed session behind as the evidence of it.

Renaming releases the old name:

```sh
nanus sessions name project-x 01a09a98-d8c4-73d7-b11d-638077efeeca
```

A session can be renamed while an agent is holding it, and the name a command writes is the
name everything shows: the store is durable immediately, and the agent re-reads the name
from it whenever it *reports* on a session — a listing, and the attachment that labels a
client's screen. Its cached copy is a probably rather than a fact, because the command that
renames a session does not connect to the link and has nothing to tell the agent through.

A name is also **resolved** through the store rather than against that cache, which is the
half that matters more: `--resume` on a name that has moved on would otherwise join the
conversation it used to belong to. Only an id is answered from memory, because an id is a
store key rather than an alias.

## Deleting

```sh
nanus sessions delete nightly
nanus sessions delete 01a09a98-d8c4-73d7-b11d-638077efeeca
```

The reference is resolved exactly as naming resolves one — a name it answers to first, then
a store key — and a reference that answers to nothing is refused rather than reported as a
deletion that removed nothing. That refusal is in the CLI rather than in the store, because
the port says deleting something absent is not an error: the caller asked for it to be gone
and it is. Reporting success for a typo would leave somebody believing a conversation is
gone while it is still on disk.

Deleting removes the session's directory, so its name and its log go together and no alias
is left pointing at nothing. It is not reversible, and nothing here knows whether an agent
somewhere is holding the session: a held session can still be saved again by the turn
writing it, which recreates the directory. Stop the agent, or attach and let it go, before
deleting one it is serving.

## What a session says about itself

A session records the configuration it was created under — the model, the reasoning effort,
the sandbox mode, the approval policy, and the release that wrote it — in its header. It also
records the model and effort on each model turn, because a session can be resumed against a
different model: the header describes how the conversation *started*, and the per-turn record
describes what produced each part of it. Resuming does not restamp the session, because the
earlier turns really were produced by the earlier configuration.

Every one of those fields is optional, and `absent` means *not recorded* rather than a
default. A session written before this existed has none of them, and it reads back with the
gap admitted instead of a plausible value invented for it. That is also why the session
format version did not move: the version decides how the *body* is read, an older build
ignores a header field it does not know, and treating a new header as an unreadable version
would have made every existing transcript unopenable.

## Reporting on a run

```sh
nanus sessions show nightly            # what it did and what it spent
nanus sessions show --json nightly     # the same figures, for a script
```

Both read the log and nothing else, so they need no model and no API key, and the same
session reports the same figures whenever it is asked. The report names the configuration
above, the turns, steps and requests, the prompt tokens split into cached and read, the
generated tokens and how many of them were thinking, a per-model breakdown when a session
used more than one, and why the last turn ended.

`nanus run --verbose` prints a one-line version of the same totals to stderr when the turn
finishes, which is the reading somebody wants while watching. The command is the reading
somebody wants afterwards, and the only one that works for a session the interface or a
service produced.

## Resuming

| Command | What it does |
|---|---|
| `nanus tui --resume <name\|id>` | Opens the interface on an existing conversation. |
| `nanus run --resume <name\|id> <task>` | Adds one turn to it and exits. |
| `nanus tui --connect --resume <name\|id>` | The same, against an agent that is already running. |

The reference is a name or a session id, and a name wins if both could match: a name is the
human-facing key, so a session somebody named is the one they meant.

Resuming is not read-only: it continues the conversation, which means it writes to the same
log. A writer therefore **claims** the session first, and a claim is held for as long as the
writer has it open — a `nanus run` for the length of its run, an agent for as long as it
holds the session. A second writer is refused, by name:

```console
$ nanus run --resume nightly "…"
nanus: session 01a0b1a2… is being written by nanus at /Users/you/.config/nanus/run/agent.sock (pid 41207); attach to it with `nanus tui --connect`
```

That is the supported way to continue a live conversation anyway: attach to the agent that
holds it, below. The claim is what makes that the *only* way, rather than the polite one.

Two things about the claim are worth knowing. It is a file beside the log (`lock`), holding
the holder's pid and a word for what it is, and a claim whose holder is gone is taken over
rather than honoured — a lock a crashed process left behind is not an owner. And it is
advisory: any process can write a session's log directly, and `nanus sessions delete` does
not consult it. What it defends against is another `nanus` — the ordinary way two writers
meet — not a deliberate one.

Reading without continuing is `nanus tui --session`, which needs no agent and no key,
because a transcript that has already been written down is just a file.

## Live sessions

An agent holds its sessions open. A `nanus service` therefore has a set of conversations
that are *running*, and a client can attach to one instead of starting its own:

```console
$ nanus service status
socket: /Users/you/.config/nanus/run/agent.sock
model: deepseek-flash
tools: 7
workspace: /Users/you/code/project
session: 01a09a9d-8aa2-7736-86a6-7c6d3dedaa7a  shared-work  idle  2 attached  3 events
```

- **A session outlives its clients.** The agent keeps holding a conversation after the
  terminal that opened it exits. That is what makes `--resume` reach the same session
  rather than a stale copy of it.
- **A turn outlives the client that asked for it.** The turn runs in its own task, owned
  by the session, so closing a terminal mid-turn no longer abandons the work.
- **A session is one conversation with many views.** Every attached client sees the same
  frames: prompts from other clients, streamed answers, tool calls, and the ending. Two
  terminals can watch one conversation.
- **One turn at a time.** A session's log can only be written by one turn, so a prompt to
  a busy session is refused with a message rather than queued — the model has not seen the
  first answer yet, and pretending otherwise would reorder the conversation.

A client that attaches mid-turn **catches up with the turn it landed in**. The frames
before it arrived went out to clients that were already there, and the store does not have
the turn yet — the log is written when a turn ends — so the agent hands it the part of the
running turn nothing else holds: the prompt, the steps, and the deltas so far, in order, as
one `Backlog` frame immediately after the attachment. The turn is then already on screen,
and the live frames continue from there rather than beginning in the middle of a sentence.

That batch is not history and does not become a second copy of it. It holds exactly what
the store has not got, it is folded where folding changes nothing (two adjacent deltas of
one kind are one piece of text either way), and it is emptied the moment the turn is
written down — so a client attaching at any instant sees a finished turn in the store and a
running one in the backlog, never both and never neither. A question the turn is waiting on
crosses the same way, because it is state rather than history: a client arriving after the
question went out is shown it, with the reason the first client was given, and can answer
it.

The agent holds at most [`MAX_HELD_SESSIONS`](../crates/nanus-link/src/server.rs) open, and
lets the least recently used *idle* one go when it needs room. The bound yields to the work:
a session that is running a turn or has a client attached is never dropped, even if that
means holding more. A session that is let go is still on disk, and attaching to it again
loads it.

## What travels over the link

A session is the agent's; a client's view of it is a handful of frames.

| Frame | Meaning |
|---|---|
| `Ready` | What the agent is — workspace, model, tool count. |
| `Attached` | Which session this connection is now a view of. |
| `Backlog` | The turn that was already running, so a client that attached in the middle of one has the whole turn rather than its tail. |
| `Sessions` | The sessions the agent is holding. |
| `User` | Somebody asked something, sent to every view but the one that asked. |
| `Text`, `Reasoning`, `Step`, `Tool`, `ToolDone`, `Usage` | The turn, as it happens. |
| `Approval` | A call outside the sandbox needs a decision; the client answers with an `approve` request. |
| `Done`, `Failed` | How it ended. |

Deliberately not a session log. A client that wants the conversation reads it from the
store, where it is already durable, rather than receiving a second copy over a socket that
would then be a second source of truth — which is why `Backlog` holds only the turn in
flight and is emptied as soon as that turn is in the log.

## Known limits

- **The claim is advisory and process-scoped.** A session is claimed for writing, so a second
  `nanus` is refused by name; a process that writes the log directly is not. A claim says
  "this *process* is writing this session", so two holders in one process — which is what an
  agent holding a session and a connection racing to open the same one would be — see one
  claim between them. That is correct for the shipped agents, which are one per process.
- **Deleting does not consult the claim.** `nanus sessions delete` removes a session another
  agent is writing, and the holder's next save recreates the directory. Stop the agent first.
- **No deletion in the interface.** `nanus sessions delete <ref>` removes a session and
  releases its name; the interface has no key for it yet, so a conversation is removed from
  the CLI rather than from the screen it is being read on.
- **Names are flat and case-sensitive.** `Nightly` and `nightly` are two names, and there
  is no namespacing.
- **A name is per store, not per machine.** `$NANUS_HOME` decides which names exist.
