# The service

One agent, running for as long as you want it to, reachable from anything on the machine
that runs as you. This is the mode for the case the other two cannot cover: an agent that
has to outlive the shell that started it.

```sh
nanus auth set deepseek    # or export DEEPSEEK_API_KEY=...

nanus service start        # detached; survives the shell
nanus service status       # is it there, and what is it
nanus service stop         # stop it, cleanly
```

## Starting it

`nanus service start` composes an agent, then runs *this same binary* again with a hidden
`--detached` flag, which puts the child in a session of its own on Unix. On Windows the parent creates
the child with `DETACHED_PROCESS` and `CREATE_NEW_PROCESS_GROUP`. Detaching is not
cosmetic: a process in the shell's process group receives the hangup when the terminal
closes, and an agent that dies with the terminal is not a service.

`--foreground` skips the detaching and serves in the terminal you ran it in. That is the
form a supervisor wants:

```ini
# /etc/systemd/system/nanus.service
[Service]
ExecStart=/usr/local/bin/nanus service start --foreground
# A detached service has no session bus and no unlocked keychain, so the environment
# is the fallback that fits here: a key stored in the file fallback works too, under
# the home systemd gives the unit.
Environment=DEEPSEEK_API_KEY=...
Restart=on-failure
```

Either way, `start` does not return until the agent answers on its socket. A daemon that
fails — no key, a workspace that is not a directory, a socket already in use — fails
*after* its parent has gone, where nobody can see it, so the parent waits, checks the
child's status on every pass, and reports the log's path when there is something in it to
read.

Starting a second service where one is already listening is an error rather than a second
daemon that cannot bind. Without that check the child would fail and the parent's poll
would find the *old* service and report success, which is the one outcome a user must
never be given.

## Stopping it

`nanus service stop` connects to the local endpoint and asks the agent to stop. It does not
send a signal. Unix returns after sending; Windows retains the pipe until `Bye` or EOF,
with a five-second deadline, so closing the client cannot discard an unread request. EOF
counts as a stopped peer even when its acknowledgement was not flushed.

`SIGTERM` and `SIGINT` are honoured on Unix; Windows foreground service processes
watch Ctrl-C. Stopping over the link works on both transports.

`nanus service status` exits non-zero when nothing is answering, so a script can branch on
it:

```console
$ nanus service status
socket: /Users/you/.config/nanus/run/agent.sock
model: deepseek-flash
tools: 7
workspace: /Users/you/code/project
session: 01a09a61-39ca-77fb-aa94-b0c0b6d8f543

$ nanus service stop && nanus service status
nanus: asked the service at /Users/you/.config/nanus/run/agent.sock to stop
nanus: no agent is listening at /Users/you/.config/nanus/run/agent.sock: No such file or directory (os error 2)
```

The `session` lines are the conversations the service is **holding open**: whether each is
running a turn, how many clients are attached, and how many events it has. Asking the
question creates none of them.

## Talking to it

An interface either connects to it explicitly or simply finds it:

```sh
nanus tui --connect                    # the explicit spelling
nanus-tui                              # a bare interface connects to the service by default
nanus tui --connect --resume nightly   # attach to a conversation it is already holding
```

All three reach the same socket. Nothing else has to know the service exists: it is an
agent on a socket, and the interface speaks the same [link](tui.md#the-link) it uses for an
agent a shell started.

**A service holds its sessions open**, which is what makes it more than a way to run the
same agent twice. A conversation survives the terminal that started it, a turn survives the
client that asked for it, and several clients can watch one session at once — each prompt
goes to every view, and each answer does too. A session is still one turn at a time, so a
prompt to a busy one is refused rather than queued. See [sessions](sessions.md).

The agent holds a bounded number of conversations and lets idle ones go when it needs room;
a session that is running or has a client attached is never dropped. Turns that are running
do interleave on the agent's single thread, because the kernel is single-threaded by design
and there is exactly one model and one toolset.

## Configuration

| Setting | Flag | Default |
|---|---|---|
| Socket | `--socket PATH` | `service_socket`, else `<nanus home>/run/agent.sock` |
| Log | `--log PATH` | `service_log`, else `<nanus home>/nanus-service.log` |
| Workspace | — | `workspace_root`, else the directory the service was started in |
| Everything else | — | the same configuration every other mode reads |

`nanus config` prints both paths resolved, which is what to check first when a client
cannot find a running service.

The log exists because a detached process has no terminal at all: its standard error
points at that file, so a startup failure is somewhere a person can read it rather than a
pipe nobody holds. Diagnostics go there at `warn` and above; `RUST_LOG=info` before
`start` makes the service say so when it begins listening.

Two services on one machine is a second socket. `--socket` selects it for `start`, `stop`,
`status`, and — with `--connect` — the interface:

```sh
nanus service start --socket /tmp/other.sock --log /tmp/other.log
nanus tui --connect --socket /tmp/other.sock
```

Without `--connect`, `--socket` is a usage error rather than a setting that is quietly
ignored: `nanus tui` binds its own socket for the agent it starts, so there is nothing for
the flag to select. For a permanent second service, set `service_socket` in a
configuration file and select the file with `--config`.

## Windows supervision

There is no Service Control Manager integration and no `LocalSystem` agent. Run the service
as the same user as the interface, either detached or with `--foreground` under a supervisor.
Task Scheduler with an **at log on** trigger can run as that user without storing a password
or requiring administrator rights; NSSM-style wrappers are another option when configured
for that user's identity. A detached process survives its shell, but ends at logoff and
is not started at boot when nobody is logged in.

The default endpoint is always computed; `run/agent.sock` and custom filesystem socket paths
are Unix endpoints. The legacy `--socket` option denotes a full local pipe endpoint on Windows.
Logs and configuration still use filesystem paths on either platform. Native Windows behavior
must be verified before treating this port as release support.

## Known limits

- **The link is local.** Unix uses a domain socket under the nanus home; Windows uses
  `\\.\pipe\nanus-<sid>-agent`, computed independently by the client and service from the
  current user's SID. Windows pipes reject remote clients and refuse a second owner.
  There is no remote mode. Native Windows verification is pending in
  [the transport workflow](../.github/workflows/local-transports.yml).
- **The link trusts its peer.** On Unix the socket is `0600` inside a `0700` directory.
  Windows uses the default pipe descriptor, whose full-control principals are the creator,
  LocalSystem, and Administrators, with read access for Everyone and Anonymous. The Windows
  link does not defend against another user on the same machine; a cross-user connection
  test is deferred and single-user sandboxed environments are assumed. The descriptor
  read-back test runs in native Windows CI. The frame cap bounds parsing, not hostile peers.
- **A session is claimed, not locked.** The agent claims every session it holds, for as long
  as it holds it, so a second `nanus` — a `run --resume`, another service — is refused with a
  sentence naming this agent's socket. The claim is a lock the operating system holds on a
  file beside the log, so a holder that dies releases it as it exits and a process that writes
  the log directly is not stopped. Attaching to the agent that holds it stays the supported way to share a session.
- **A session is not *locked*, but a name is re-read rather than trusted.** The agent
  refreshes a held session's name from the store for every listing and attachment, and
  resolves a name through the store rather than against its own copy, so a rename made by
  `nanus sessions name` is visible everywhere at once.
