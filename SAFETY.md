# Safety notice

`nanus` is an agent harness: it gives a language model the ability to read and
write files and to run programs on your machine. Read this before running it.

## What it can do

With the default toolset, a model driving `nanus` can:

- **Read any file** inside the configured workspace root.
- **Write and edit files** inside the configured workspace root.
- **Run programs** as your user, with your environment and your permissions.

There is no sandbox that confines what a program you agreed to run may then do.
A shell command that writes outside the workspace root is outside the workspace
root.

## Defaults, and what they do not protect you from

Two independent settings govern how much freedom the model has. The **sandbox mode**
is the standing permission — what a tool call may do without anyone being asked —
and the **approval policy** decides what happens to a call *outside* it.

**Sandbox mode** (`read_only` by default).

- `read-only` — reads run; a write or a program needs approval.
- `workspace-write` — reads and writes inside the workspace root run; a program
  needs approval, because a workspace root cannot confine what a program does.
- `danger-full-access` — every call runs; nothing needs approval.

**Approval policy** (`per_call` by default).

- `per_call` — a call the sandbox does not permit prompts you. If no answerer is
  available, or the prompt cannot be delivered, the call is **denied** rather than
  allowed. Approval is one-shot unless you choose otherwise.
- `permitted` — a call that cannot destroy anything runs without asking. A call
  that looks destructive — a command that deletes or overwrites — still prompts,
  *unless* every path it names is inside a temporary directory, where a destructive
  command is the ordinary way to clean up. Prefer this over `per_call` when you
  want the agent to work without being asked about every read-only shell command,
  but still want a person in the loop for a deletion.
- `all_calls` — every call the sandbox does not permit runs without asking. This is
  a free for all, and it is only safe where the *environment* is the containment:
  a container, a virtual machine, or a machine whose contents are disposable. Do
  not choose it on a laptop with your work on it, and note that it does not
  enforce anything itself — it removes the gate rather than adding a wall.

Every prompt offers a standing "always allow", which records the tool for the rest
of the session. It is per session and in memory, and it is wider than one call:
granting `bash` means every later `bash` call in that conversation runs without
asking, including a destructive one.

The state is shown in the interface's status line and chosen with `Shift+Tab`;
it can be chosen at startup with `--approval`. A state chosen in the interface
reaches the agent that owns the gate, so it is not merely cosmetic.

A tool's declared access decides which of the three sandbox questions it is: the
read tools and the search tools read, `write` and `edit` write, and `bash` runs a
program.

Who is asked depends on where you are. `nanus run` prompts on the terminal, and
only when stdin is a terminal: a redirected stdin is not an answerer, so a script
cannot approve by accident. The interactive interface draws the question over the
conversation, and the agent asks over the local link — every client attached to
the session sees it and the first answer settles it. A service with no client
attached has nobody to ask and therefore denies.

`danger-full-access` is never sent to the model provider as a request; it is a
local decision, and it means what it says.

### The honest caveats

- **A non-zero exit code is not an error**, by design: the command ran and told you
  what it thought. Do not read a successful tool call as a successful command.
- **Approval policy is not a sandbox.** An approved `sh -c` command can do
  anything your user can. Approval asks whether to run it, not what it will do.
- **Sandbox mode confines the filesystem, not the network and not the process
  table.** A command may reach the network. That is outside the model's vocabulary
  but inside the process's capability.
- **The model sees what it reads.** A file you let it read is a file whose contents
  leave your machine in the next request to the provider.

## Your own commands: the `!` escape

The interactive interface has one escape hatch that does not go through the model. A
prompt that opens with `!` is a shell command **you** are running: the interface echoes
it, runs it with `sh -c` in the directory it was started in, and draws what came back.

This is not the agent acting, and it is worth being clear about what it therefore is
not:

- **It is not recorded.** The session log is the model's history — what it was told and
  what it said — and a command the model neither asked for nor is shown is not part of
  it. Nothing about a `!` command reaches the transcript the model is given, and the
  model cannot see what you ran.
- **It is not confined.** The tools are rooted at the workspace; your shell is rooted
  where you are. `!cd /` is a shell command, and so is `!rm -rf` with the path of your
  choosing. It is exactly what typing the same line into your terminal would do.
- **It is not approved, because there is nobody to ask.** Approval exists because a
  *model* asked for a call and a person should decide. You are the person, and you
  typed it.
- **It waits, and it can be stopped by leaving.** The command runs as a task so the
  interface keeps drawing, but there is no key that kills it: the interface is not its
  parent's supervisor, and a command that hangs hangs until it finishes or you quit.
- **Its output is bounded.** The first rows are drawn and the rest counted, because a
  command that prints a great deal should not push the conversation out of reach.

If you want a command to be recorded, sandboxed, and visible to the model, ask the
model to run it — that is what the `bash` tool is, and it goes through the gate above.

## Secrets

- The API key is read from the `DEEPSEEK_API_KEY` environment variable.
- Configuration is stored in a file; the key is **never** written to it, and the
  configuration's `Debug` rendering redacts it, because that rendering reaches logs.
- Session transcripts are stored under `$NANUS_HOME/sessions/`. A transcript
  contains everything the model saw and produced, including any secret it read.

## The agent's socket

An agent serving an interface — or a service — listens on a Unix domain socket under
`$NANUS_HOME/run/`. The socket is created `0600` inside a `0700` directory, so only
your user can connect to it, and it is a local socket: nothing listens on an address
and no packet reaches a network interface.

The trust boundary is *processes running as you*, and it is worth being precise about
what that means. A program that can connect to the socket can send a prompt to an
agent that reads and writes files and runs programs with your permissions. Such a
program could already do all of those things itself — it runs as you — so the socket
grants it no new capability. What it does grant is *plausible deniability*: work done
through the socket is recorded in the session log under a session the agent created,
not under the caller's name. Treat the log as the record of what was asked, and
remember that anything running as you could have added to it.

Two consequences worth stating plainly:

- **A service is reachable by anything running as you, for as long as it runs.** Stop
  it when you are done, or run it under a supervisor that does.
- **Nothing about a service is remote.** There is no port, no TLS, and no
  authentication, because there is no remote mode to secure. Do not add one by
  forwarding the socket.

## Prompt injection

A harness that reads files and fetches content will eventually read text written by
someone who wants it to do something else: a `README.md`, a code comment, a test
fixture, a web page. Treat the model's instructions as untrusted input, not as your
instructions. Concretely:

- Prefer `read-only` or `workspace-write` over `danger-full-access`.
- Prefer `per_call` (the default) or `permitted` when a human is watching, and keep
  `all_calls` for an environment that contains the blast radius.
- Review what a run did, not only what it said. The session log is the record.

## Reporting

This is a developer preview. Report a safety issue through the repository's issue
tracker rather than in a public discussion of an exploit.
