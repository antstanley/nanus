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

An append-only log of events — turns, messages, tool calls and their results, and the session's
goal — plus the identity of the conversation: a store key, a creation time, the directory it ran
in, and the harness configuration it ran under. It is the only copy. Everything a client shows
is derived from it, which is why the agent records before it answers and why a transcript is
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

A session is written at the lowest body version that holds it: version 4 when a user message
carries typed content, version 3 when it has enabled [managed context](context-management.md), and
version 2 otherwise, so an older build can still open what it can read faithfully. The two are
independent: a version-4 header carries `"managed":true` when the session is managed, because the
version no longer implies it, and no lower header may carry it. Readers — the store's listing as
well as a load — accept versions 1 to 4; old
text logs keep their sequence, usage and optional provenance without inferred pixels. Version 1
cannot introduce typed content or signed replay via extra fields. Version 2 stores ordered image
blocks inline beside display summaries, so resuming does not need the original image file. Signed
Messages assistant blocks are retained for unchanged-prefix replay. Older binaries cannot read v2
bodies; there is no destructive bulk migration or automatic downgrade.

The local Responses preparation additionally validates `openai.responses` replay in v2
records. Its closed reasoning/message/function subset retains original ciphertext, phase,
annotations and ordered call identities; exact neutral text/calls must agree on reload.
Annotations grant no tool, URL or filesystem authority. Optional closed `context_receipt` binds
source/body hashes and exact fitting counts/budget; absent fields preserve the previous envelope.
The pure request preparer checks prior receipts against complete immutable original history and
controls before replaying original items, including after whole-turn elision. Opaque-only completed
Responses output remains in the surface fold. These are consistency hashes, not ciphertext
signatures. Explicit library opt-in selects this source/body-aware transport. Pure estimation may
substitute only final balanced batch result values; dispatch requires exact source. Default stock
composition stays unchanged; publication/adoption and live/native acceptance are not established.

An additional default-off `set_instruction_revisions(true)` adapter policy permits trusted
leading-System changes at new user turns in stateless Responses. Original contexts retain a
version-1 instruction snapshot (at most 64 texts and 256 KiB serialized, including escaping),
with its ordered-array digest. Current instructions travel in the new request; old receipts stay
bound to their original instructions and unchanged nonprompt controls. The tagged source hash
retains complete user/assistant/tool history and earlier snapshots. Mid-turn changes, mixed legacy
histories and missing/altered evidence refuse. Legacy default mode and its serialized hashes stay
unchanged; opting in never manufactures snapshots for existing responses. Complete snapshots count
against decoder/record/session limits. These consistency receipts authenticate no external file or
ciphertext. Downstream consumer and live acceptance are separate from the local adapter tests.

Direct user input retains the same ordered typed blocks as tool results. `Message::user_with_content`
validates before deriving a display summary; provider adapters send the blocks themselves. Text-only
constructors preserve their original JSON. Version 4 prevents older readers silently dropping user
images, and is written only when a session holds some; versions 1 to 3 refuse typed user fields
even when null. No CLI or link image upload surface
is introduced. Library hosts may compose direct multimodal requests and store typed user events.

`try_to_jsonl` validates content and 4 MiB records/64 MiB total logs. The store uses it before
atomic replacement and bounds reads before parsing, leaving the existing log intact on failed
save. The legacy infallible `to_jsonl` remains for trusted in-memory compatibility; hosts should
use the fallible writer. CLI/link projections show summaries, never duplicate image blobs.
The runner mutates memory; hosts own saving before success/Done acknowledgment.

A session that enables [managed context](context-management.md) is written as body version 3 — or
4, marked managed, once it also holds typed user content — and also retains its context mode,
revision and decision records, archive receipts, request-attempt records and recovery records. A
body that is not managed and carries one of those records is refused rather than read. The model's effective request is a derived
view and never replaces the transcript. A managed host checkpoints accepted events and projection
changes before the next model request uses them, so a checkpoint can hold a settled open turn: it
is not a completed answer. Enabling upgrades a body through that checkpoint; nothing downgrades
one. Archived shell output lives beside the log, under the session's own directory:

```text
    01a09a98-.../
      artifacts/
        <uuid>.raw         a finalized capture, named by its artifact/published receipt
        <uuid>.partial     staging, or an orphan nothing references
```

## Naming

A name is how a session is found again, so it is taken for good: starting a second session
with a name that is already held is refused, not moved. Silently reassigning an alias would
make `--resume nightly` open somebody else's conversation.

Two decisions are worth stating, because both are about the same failure — a reader opening a
conversation they did not mean:

- **A name is one word, not a path.** There are no namespaces: a name is an alias for one
  store key, the store is one flat directory, and a `/` in a name would suggest a tree that
  does not exist. A user who wants grouping writes it into the name (`project.nightly`,
  `project:nightly`), because the punctuation is part of the word rather than a level.
- **Case does not make a second name.** `Nightly` and `nightly` are one name: naming a second
  session with the other case is refused and names the session that holds it, and resolving
  either spelling finds that session. What is *stored* is the spelling the session was named
  with, so a rename is how a name changes case and a listing shows what a person typed.
  Leading and trailing whitespace is trimmed on the way in, because a name nobody can see is a
  name nobody can type back.

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

Deleting removes the session's directory, so its name, its log and its archive go together and
no alias is left pointing at nothing. It is not reversible.

Deletion requires exclusive session ownership: a session held by another writer is refused, by
name, exactly as a second writer is. The deletion retires the session's id and moves the whole
directory into the store's trash in one step, then removes the bytes and reclaims their archive
quota. A stale save — a writer that loaded the session before it was deleted — cannot recreate
it: a save, a checkpoint or a claim for a retired id is refused. A crash part way through is
finished the next time the store opens, and never reversed. Names continue to be aliases rather
than identity, and go with the session. This applies to every session, legacy ones included.

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

A goal is the one addition to the *body*: a `goal/change` record, written wherever a goal is
set or moved. The version stayed put for it too, and deliberately. A newer build reads every
older log unchanged, because an older log simply has no such record; an older build meeting a
log with one refuses it as a malformed event rather than guessing, which is the answer the
version would have given anyway — and moving the version would have made the newer build's
own reader refuse every log written before the goal existed, since it checks for an exact
match.

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

Two things about the claim are worth knowing. It is a file beside the log (`lock`) that the
operating system locks while the holder has it open, labelled with the holder's pid and a
word for what it is. The lock is what decides — it is atomic, so two writers starting at the
same instant cannot both be first — and the kernel releases it when the holder exits, however
it exits, so a lock a crashed process left behind is not an owner anybody has to detect.
Session deletion consults exclusive ownership and retires the session id. The claim remains
advisory against unrelated same-user programs that edit files directly; supported `nanus`
deletion and save operations cannot race to recreate a retired conversation.

A resumed managed session validates its last committed projection and artifact references
before it runs. A checkpoint that holds an open turn is closed as interrupted under the writer
claim before new work is admitted: a recovery record, an `unknown_dispatch` outcome for every
request intent that never finished, and the turn's end. Nothing reruns an old tool call or treats
a saved request intent as a completed response. Missing artifacts are reported as unavailable
evidence. An invalid projection refuses managed continuation until the session's context is
explicitly reset to legacy replay, or the store is repaired.

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
the turn yet — a legacy log is written when a turn ends, and a managed one at each checkpoint —
so the agent hands it the part of the
running turn nothing else holds: the prompt, the steps, and the deltas so far, in order, as
one `Backlog` frame immediately after the attachment. The turn is then already on screen,
and the live frames continue from there rather than beginning in the middle of a sentence.

That batch is not history and does not become a second copy of it. It holds exactly what
the store has not got, it is folded where folding changes nothing (two adjacent deltas of
one kind are one piece of text either way), and it is emptied the moment the turn is
written down — so a client attaching at any instant sees a finished turn in the store and a
running one in the backlog, never both and never neither. Neither is a guess: the attachment
carries the log's own position when the batch was taken, and the client reads the log
afterwards, so a turn that ended in between is recognised as already recorded and the batch
that carries it is dropped rather than drawn on top of the log. The two reads are not one
instant, and that number is what makes them agree. A question the turn is waiting on
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
| `Goal` | The session's durable objective, or its absence: sent on attaching to a session that has one, on every change, and in answer to a `goal` request. |
| `ContextStatus` | A managed session's context status: when a step's request is prepared, on a `context status` request, and after a reset. |
| `ContextDecision` | A context proposal accepted, rejected or cancelled, or an automatic fit. |
| `Checkpoint` | A managed session's checkpoint was acknowledged: the durable frontier moved. It ends nothing. |
| `Done`, `Failed` | How it ended. |
| `Refused` | A request that is not a prompt — a model, provider, credential, or goal change — was not carried out. It ends no turn, which is why it is not `Failed`. |

Deliberately not a session log. A client that wants the conversation reads it from the
store, where it is already durable, rather than receiving a second copy over a socket that
would then be a second source of truth — which is why `Backlog` holds only what the store does
not yet have.

Attachments identify an immutable durable event frontier. `Attached` and `Backlog` carry the same
stream mark — the session's stream epoch, its frame watermark, and the frontier's event count and
digest — taken in one step with the viewer's registration. A client replays the store only
through that frontier, clipping a newer file to that prefix and refusing a shorter or mismatching
one with a visible notice, then applies the backlog and the frames that follow. Checkpoint
completion advances the frontier and retires only the backlog segments it covers, in the same
step and before any new progress; existing viewers keep their visible stream, because a
checkpoint does not send `Done` — the `Checkpoint` frame only moves their watermark. Context
frames carry the session's own monotonically increasing frame id, so a duplicate or older one is
ignored, and a frame from another stream epoch is refused. Context status and decisions have
their own frames and stay distinguishable from assistant output and goal changes. The protocol
is version 10; an older peer is refused before any request.

## Known limits

- **The claim is advisory and process-scoped.** A session is claimed for writing, so a second
  `nanus` is refused by name; a process that writes the log directly is not. The lock is held
  for *the process* that took it, so two holders inside one process — which is what an agent
  holding a session and a connection racing to open the same one would be — are one claim
  rather than two, and the store answers a re-claim from the process that already has it.
  That is correct for the shipped agents, which are one per process.
- **No deletion in the interface.** `nanus sessions delete <ref>` removes a session and
  releases its name; the interface has no key for it yet, so a conversation is removed from
  the CLI rather than from the screen it is being read on.
- **Names are flat, and case does not distinguish them.** `Nightly` and `nightly` are one
  name and resolving either spelling finds it, while the spelling a session was named with is
  what a listing shows. There is no namespacing: grouping lives in the name itself.
- **A name is per store, not per machine.** `$NANUS_HOME` decides which names exist.
