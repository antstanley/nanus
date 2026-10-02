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

An embedding host can install an exact-call `ToolPolicy` for registered tools, including
reads. Errors/cancellation deny; a one-call grant cannot bypass registry validation. The
policy adds application scope checks, not OS confinement. A controlled turn races its sticky
signal against waiting work and drops interrupted futures; the host must terminate detached
process groups or other side effects. Inline image bytes are verified/bounded and unknown
model profiles are refused before image I/O/HTTP. The library performs no URL fetch or resizing.

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
it, runs it with `sh -c` on Unix or `cmd /S /C "<line>"` on Windows in the directory it was
started in, and draws what came back. On Windows the line is handed to `cmd` exactly as typed,
quotes included, because `cmd` does not undo the escaping a program's arguments are given.

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
- **It does not read your keyboard.** Its standard input is closed rather than handed
  the terminal, because the terminal's keystrokes are the composer's — a command that
  read them would take characters out of the prompt you are typing. A command that wants
  input takes it from a file or a pipe, and one that reads anyway gets an end of input
  immediately rather than waiting for a `Ctrl+D` that belongs to the interface.
- **It waits, and it outlives the interface.** The command runs as a task so the
  interface keeps drawing, but there is no key that kills it, and nothing here is its
  supervisor: a command that hangs hangs until it finishes, and one still running when
  you quit keeps running after you have left. A command that should stop with the
  interface is not one to run here.
- **Its output is bounded.** The first rows are drawn and the rest counted, because a
  command that prints a great deal should not push the conversation out of reach.

If you want a command to be recorded, sandboxed, and visible to the model, ask the
model to run it — that is what the `bash` tool is, and it goes through the gate above.

## Secrets

A provider key is the one value that must not reach a log, a transcript, or a
process list. `nanus auth set <provider>` stores one, `nanus auth clear <provider>`
removes it, and `nanus auth status` reports which providers have one without
printing any value.

- The stores are tried in order: **the macOS keychain**, then **a `0600` file**
  under `$NANUS_HOME/secrets/` (its directory is `0700`), then **the provider's
  environment variable** (`DEEPSEEK_API_KEY`, `ZAI_API_KEY`, `ANTHROPIC_API_KEY`,
  `OPENAI_API_KEY`). The first store holding a value answers. A store that cannot
  answer — a locked keychain on a detached service — does not hide a value another
  store holds, which is the case a service actually runs in.
- The value is read from **standard input**, never from an argument, so it does
  not appear in `ps` for the life of the command.
- **One exception is stated rather than hidden:** the macOS keychain write runs
  `/usr/bin/security add-generic-password`, which accepts the value only as an
  argument or as a terminal prompt and does not read standard input (verified
  against the shipped tool). So for the few milliseconds that process runs, the
  value is in its argument list, readable by anything running as this user — who
  could already read the keychain entry itself. The alternative, a file the tool
  would have to read, is a worse place to leave a key.
- Configuration is stored in a file; a credential is **never** written to it, and
  the configuration has no field that could hold one. The wrapper a credential is
  carried in redacts its own `Debug`, and the read is a method named `expose` so
  every site that handles a raw value is greppable.
- Session transcripts are stored under `$NANUS_HOME/sessions/`. A transcript
  contains everything the model saw and produced, including any secret it read.

The file store's `0600`/`0700` permissions apply on Unix. On Windows, files inherit
the containing directory's ACL; the backend does not set an explicit descriptor.
Choose a home directory that other users cannot read, or use environment credentials.

What this does **not** do: the file store is not encrypted, so a secret in it is
readable by anything running as you — as the environment variable already was. The
point of the store is to get a key *out* of the environment and out of the
configuration file, not to defend it from the user's own processes.

## The agent's local link

On Unix, an agent serving an interface — or a service — listens on a domain socket under
`$NANUS_HOME/run/`. The socket is created `0600` inside a `0700` directory, so only
your user can connect to it, and it is a local socket: nothing listens on an address
and no packet reaches a network interface.

On Windows the local endpoint is a SID-named pipe, with remote clients rejected and the
first instance refusing an existing owner. A pipe has no private directory to live in: its
name is global and computable by anyone who knows your SID, and the default descriptor grants
full control to the creator, LocalSystem, and Administrators and read access to Everyone and
Anonymous. So the pipe alone does not say who is at the other end, in either direction:

- **Another account could own the name first**, and an interface would then be talking to it
  — prompts, and a credential set from the interface, included.
- **Another account can open the agent's pipe** and hold the connection.

Both ends therefore prove themselves before a frame is exchanged. When an agent owns its pipe
name it writes a fresh random key to `%LOCALAPPDATA%\nanus\run\<pipe name>.key`, a directory
no other account can read, and removes it when it stops. The client challenges the agent, and
sends nothing more until the agent answers with an HMAC-SHA-256 over two fresh nonces made with
that key; then it answers the agent's challenge the same way. The key never crosses the pipe. A
squatter cannot answer, so the client refuses it with a sentence and sends it nothing; a client
that cannot answer is dropped within five seconds, and the agent goes on serving. The handshake
lives in `nanus-link/src/transport/guard.rs` and is tested on every platform; the Windows tests
add a real squatter and a real stranger.

What this does not do: an administrator, or anything running as LocalSystem, can read the key
and so pass for you — as it could already read your workspace. And another account can still
open connections faster than they are dropped, which slows the agent's accepts; a refused accept
is logged and retried rather than stopping the agent, so it does not end turns in flight. The
endpoint name is checked exactly, as `\\.\pipe\nanus-` followed by letters, digits, and hyphens,
because a name with `..` in it would otherwise pass a prefix check and resolve to another
machine.

The ACL read-back, local pipe behavior, Job Object grandchild cleanup, and detached-service
lifecycle passed [native Windows validation](https://github.com/antstanley/nanus/actions/runs/36852736607)
at `b13b9a9`. The handshake postdates that run.

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
- **Nothing about a service is remote.** There is no port and no TLS, because there is
  no remote mode to secure; the Windows handshake proves only that both ends are the same
  local user. Do not add a remote mode by forwarding the socket or the pipe.

## The shell on Windows

On Unix the `bash` tool runs `sh -c` as the leader of a new process group, and only a timeout
or shutdown kills the group: a command that starts `server &` and exits leaves it running. On
Windows the tool runs `cmd /S /C "<script>"` inside a kill-on-close Job Object, and the job is
the call's. When the command exits, **everything it started ends with it** — a `start /b
server`, and a `nanus service start` run as a tool, whose service is started inside the job.
This is deliberate: it is what a job is for, and it is also what closes the pipes a leftover
process would hold open. Run a long-lived process from your own terminal, not through the
agent. Children get no console window, so a service running detached does not open one on the
desktop for each command.

A `nanus service start` from your own terminal asks to leave any job its launcher is in, so a
service started from an SSH session or a CI step survives it. A job that forbids leaving is
obeyed, and the service then lasts as long as that job.

## The one Windows binding outside `winsafe` and `process-wrap`

The detached launcher clears inheritance on its own standard handles, which no maintained crate
offers a safe call for. The one declaration it uses, `SetHandleInformation`, comes from
`bun_windows_sys`, a dependency-free binding leaf published by the
[Bao](https://github.com/putao520/bao) project (not Bun). It was audited against the Win32
signature: the arguments are a handle and two integers, nothing is dereferenced, and failure is a
return value. The crate is pinned to `=0.1.0`, and CI fails if the version moves or if any other
item from it is used, so an upgrade or a second use is a new review rather than a silent one.
Writing the declaration in this repository instead would need `unsafe`, which no crate here may
contain.

## Prompt injection

A harness that reads files and fetches content will eventually read text written by
someone who wants it to do something else: a `README.md`, a code comment, a test
fixture, a web page. Treat the model's instructions as untrusted input, not as your
instructions. Concretely:

## `read_video`

An optional tool, off by default (`read_video = true`). It runs `ffprobe` and `ffmpeg`, so it
declares `Execute` access and the approval policy applies to it exactly as to `bash`; it is never
labelled a read.

- **What runs.** A fixed pair of executables, taken from `ffmpeg_dir` or `PATH` at startup, with
  structured arguments, no shell, no stdin and a cleared environment. Nothing in a model's
  arguments reaches a command line: the source is copied to a temporary file with a fixed name, and
  the model chooses only a path, an interval, a frame count and a question.
- **What it will not read.** The path goes through the rooted filesystem port, so a path outside the
  workspace is refused. FFmpeg is run with `-protocol_whitelist file`, and a container that refers to
  other files or streams (concat lists, HLS and DASH playlists, image sequences) is refused.
  A protocol whitelist alone does not confine a decoder: **this is not an OS sandbox**, and a
  hostile file is still parsed by FFmpeg as your user. Treat videos from untrusted sources the way
  you treat any file you open in a media player, and keep `per_call` approval if that matters.
- **What it bounds.** Source size and duration, dimensions, frame count and size, decoder output,
  per-process and whole-call time, and result text. A dropped call kills the decoder, and the
  temporary copy is removed on every exit path.
- **What leaves the machine.** Stills, never the video or its audio, and only to the provider already
  in use. With `analyze`, up to four JPEGs go to a vision model on the **same provider, plan,
  endpoint and credential account** as the conversation; the credential is read by account name and
  a key stored for one plan is never sent to another. This is a second, paid request that the
  main-loop token and goal budgets do not count, so it has a budget of its own
  (`video_analysis_budget`): reserved before the request is sent, settled to the usage the provider
  reports, and kept whole when the request fails, reports nothing or is cancelled, because cancelling
  locally does not stop the provider generating. It is one request with no retry, and the manifest
  records the reported usage.
- **What a result is.** Text inside a video, and the analysis model's answer, are untrusted data. The
  analysis is told so, and the result labels it as a model's reading of sampled stills.

- Prefer `read-only` or `workspace-write` over `danger-full-access`.
- Prefer `per_call` (the default) or `permitted` when a human is watching, and keep
  `all_calls` for an environment that contains the blast radius.
- Review what a run did, not only what it said. The session log is the record.

## Reporting

This is a developer preview. Report a safety issue through the repository's issue
tracker rather than in a public discussion of an exploit.
