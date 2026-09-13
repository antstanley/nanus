# The service

One agent, running for as long as you want it to, reachable from anything on the machine
that runs as you. This is the mode for the case the other two cannot cover: an agent that
has to outlive the shell that started it.

```sh
export DEEPSEEK_API_KEY=...

nanus service start        # detached; survives the shell
nanus service status       # is it there, and what is it
nanus service stop         # stop it, cleanly
```

## Starting it

`nanus service start` composes an agent, then runs *this same binary* again with a hidden
`--detached` flag, which puts the child in a session of its own. Detaching is not
cosmetic: a process in the shell's process group receives the hangup when the terminal
closes, and an agent that dies with the terminal is not a service.

`--foreground` skips the detaching and serves in the terminal you ran it in. That is the
form a supervisor wants:

```ini
# /etc/systemd/system/nanus.service
[Service]
ExecStart=/usr/local/bin/nanus service start --foreground
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

`nanus service stop` connects to the socket and asks the agent to stop. It does not send a
signal and does not wait for the acknowledgement: the agent finishes the frame it is
writing, removes its socket, and exits, and a `stop` that hung whenever the agent exited
before flushing would be worse than one that reports what it asked for.

`SIGTERM` and `SIGINT` are honoured too, because that is what every supervisor sends and
what Ctrl-C in `--foreground` means.

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

The `session` in that output is the status connection's own; sessions are per connection,
and a status request does not create one on disk.

## Talking to it

An interface either connects to it explicitly or simply finds it:

```sh
nanus tui --connect    # the explicit spelling
nanus-tui              # a bare interface connects to the service by default
```

Both reach the same socket. Nothing else has to know the service exists: it is an agent
on a socket, and the interface speaks the same [link](tui.md#the-link) it uses for an
agent a shell started.

More than one client can be connected at once — that is the point of a service — and each
gets its own session. Turns interleave on the agent's single thread, because the kernel is
single-threaded by design and there is exactly one model and one toolset.

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

## Known limits

- **The socket is local and Unix-only.** There is no remote mode and no Windows support:
  the workspace already depends on `nix` for process-group signalling, so Windows was
  never a target, and the link is a Unix domain socket.
- **The link trusts its peer.** The socket is `0600` inside a `0700` directory, so only
  the same user can connect — and a process running as that user can already read the
  workspace and the session log. The frame-size cap is about not handing unbounded
  *parsing* to a confused peer, not about defending against a hostile one.
- **Reconnecting is a new conversation.** A client that loses its connection starts a new
  session, because a session belongs to a connection. Resuming a named session over the
  link is not implemented; the transcript is on disk and `nanus tui --session` reads it.
