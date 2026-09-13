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

Two independent settings govern how much freedom the model has.

**Approval policy** (`ask` by default).

- `ask` — a tool call that needs approval prompts you. If no answerer is available,
  or the prompt cannot be delivered, the call is **denied** rather than allowed.
  Approval is one-shot: answering once does not approve the next call.
- `never` — every call that would need approval is **rejected immediately**,
  without consulting anyone. This is not "approve everything". It is the setting to
  choose for unattended runs, where nothing can answer and therefore everything
  that asks must fail.

There is deliberately no "auto-approve" or "always allow" policy.

**Sandbox mode** (`workspace-write` by default).

- `read-only` — writes are refused.
- `workspace-write` — writes are confined to the workspace root.
- `danger-full-access` — confinement is bypassed entirely.

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
- Prefer `ask` when a human is watching.
- Review what a run did, not only what it said. The session log is the record.

## Reporting

This is a developer preview. Report a safety issue through the repository's issue
tracker rather than in a public discussion of an exploit.
