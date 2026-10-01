# Design note: reaching an agent from another machine

**Status: proposed. Nothing here is implemented.** Today there is no remote mode, and the
docs say so on purpose: [the service page](service.md#known-limits) records that the link
is local and trusts its peer, and [features](features.md) that there is "no authentication
— there is no remote mode to secure". This note describes how to add one without disturbing
either half of that sentence for a local user, and it is what ending the "no remote mode"
stance would have to be written against.

## The decision

Remote access is **a pair of relay processes that carry the existing local link**, not a new
transport inside the agent or the interface.

```
nanus-tui ──local link──▶ remote client ══ tunnel ══▶ gateway ──local link──▶ agent
 (unchanged)              (listens locally)           (beside the agent)      (unchanged)
```

- The **gateway** runs on the machine with the agent. It accepts authenticated remote
  connections, and for each one opens an ordinary local link connection to the agent —
  the socket on Unix, the pipe on Windows, per
  [the transports note](link-transports.md) — and copies bytes in both directions.
- The **remote client** runs on the machine with the person. It listens on a *local*
  endpoint of its own and tunnels each connection to the gateway. `nanus-tui` is pointed at
  that endpoint and cannot tell the agent is remote.

Neither the agent nor the interface learns that a network exists. That is the point, and it
is what keeps three existing invariants true: the core does not depend on the interface;
the link's frame vocabulary is the interface's only view of a turn; and a session is the
agent's, with a connection merely a view of one.

## Remote users are the local user

For this version a remote user is **fully equivalent to the local user**. There are no
roles, no read-only viewers, and no per-user policy. Whoever the gateway admits can do
everything a local interface can: prompt, approve a tool call, change the approval state,
attach to any session, and stop the service.

That is a large grant, and the note states it plainly because the toolset includes `bash`
and `write`. A gateway is **remote code execution as the agent's user**, and its
authentication has to be treated as the only thing between the network and a shell. Read
[`SAFETY.md`](../SAFETY.md) with that in mind: the sandbox is *reported, not OS-enforced*,
and approval is a prompt a remote peer can answer.

Because roles are out of scope, the gateway is a **byte relay with no knowledge of
frames**. It copies lines; it does not parse `Request` or `Frame`. This is the simplest
thing that is correct, and it has a nice consequence: the gateway does not change when the
frame vocabulary does, and `PROTOCOL_VERSION` negotiation happens end to end between the
interface and the agent, as it does locally. The cost is that it cannot enforce policy —
see [what a relay cannot do](#what-a-relay-cannot-do).

## Authentication

Authentication is **mutual TLS with pinned keys**, whichever tunnel is used. Neither a
password nor a bearer token is acceptable as the sole mechanism: a leaked token is a shell.

- The gateway holds an identity key and a list of authorised client public keys, stored
  under the nanus home with owner-only permissions, like the credential file fallback.
- The client pins the gateway's key on first use, or is given it out of band, and refuses
  a gateway whose key changes. A changed key is an error with a message, not a prompt.
- Enrolling a client is an explicit act on the machine that hosts the agent
  (`nanus remote authorize <key>` or similar), never a thing a connecting client can do for
  itself.
- A key is not a credential in the sense of [`SecretPort`](../SAFETY.md), but it is held to
  the same rules: never logged, never serialised into `NanusConfig`, never in `Debug`.

The local link's trust came from the filesystem, and a network has no filesystem to lean on.
That is why this section exists, and why "we will add a token later" is not a design.

## The tunnel

The relay does not care what carries the bytes, and the choice is deliberately left to a
later decision because it is the cheapest part to change.

| Tunnel | For | Against |
|---|---|---|
| **SSH** forwarding the local endpoint | Needs no code; SSH's authentication and host-key handling are already trusted. A baseline to beat. | Unix-socket forwarding is awkward on Windows; every user needs SSH access to the host. |
| **QUIC** (`quinn` + `rustls`) | One connection carries many streams without head-of-line blocking; resume and roaming; mutual TLS built in. | Largest dependency tree; UDP is blocked on some networks; heavier to build. |
| **TCP + TLS** | Simple, passes firewalls, `rustls` only. | No multiplexing: one connection per interface. |
| **WebSocket over TLS** | Passes proxies. | An extra framing layer for no benefit to a byte relay. |

The traffic is text frames; the model's own latency is hundreds of milliseconds to
seconds, so none of these differs in a way a person would feel. The choice is about
firewalls, dependencies, and whether multiplexing is wanted. QUIC's roughly 1–3 MB and
dependency tree (estimates, to be measured) land in the **relay binary only**, behind a
Cargo feature, never in `nanus`, never in `nanus-tui`, and never in the minimal embedded
runner.

A QUIC stream is `AsyncRead + AsyncWrite`, so it drops into the relay unchanged.

## Where it lives

A new crate, `nanus-remote`, that depends on `tokio`, a TLS library, and `nanus-link`'s
**client and path helpers only** — not the `server` feature. It must not depend on
`nanus-bundle`, and `nanus-bundle`, `nanus-cli`, and `nanus-tui` must not depend on it. The
architecture rule is that dependencies point inward and the manifests enforce it; this adds
a leaf, not an edge.

Two commands, in whichever binary owns remote access:

```sh
nanus remote serve      # the gateway, beside the agent
nanus remote connect    # the local proxy; prints the endpoint to point nanus-tui at
```

`nanus-cli` already starts agents; whether `remote` belongs there or in a binary of its own
is an open question below, and it should be decided with the dependency rule in view.

## What a relay cannot do

- **Enforce policy.** It cannot tell a viewer from a controller, because it does not read
  frames. Roles require a protocol-aware gateway, which couples it to the vocabulary. That
  is deferred, not forgotten.
- **Know who connected, to the agent.** The agent sees an ordinary local connection. A
  session claim that refuses a second writer names the holder's socket, not a remote
  person. Identity at the gateway is available for logging, and the audit trail is the
  gateway's to keep.
- **Be a defence in depth.** If the gateway is compromised, so is the agent's user.

## Limits at the edge

The agent's frame cap, `MAX_FRAME_BYTES` at 4 MiB, exists "about not handing unbounded
*parsing* to a confused peer", not to resist a hostile one. A gateway faces the network, so
it adds its own limits and does not borrow the local reader's:

- a maximum line length, enforced while copying, so a peer cannot make it buffer;
- a cap on concurrent connections and on unauthenticated handshakes in flight;
- an idle timeout that closes a connection that has said nothing, and a handshake timeout;
- a refusal to start when it would listen on a non-loopback address without authentication
  configured. There is no "insecure" flag.

## Sessions, reconnects, and a turn in flight

A session belongs to the agent, and a turn runs in its own task so it outlives the client
that asked for it. A network failure is therefore just a client going away: the turn keeps
running and the session is intact on disk, and the client **re-attaches** with the existing
`Attach` request. `Done` already means the session is recorded, so a client that dropped
mid-turn finds the result after it reconnects.

The remote client should reconnect on its own and re-present the local endpoint's
connection, but it cannot replay frames it missed: whatever the agent's attach replay gives
a newly attached client is what a reconnected one gets. If that proves too little for a slow
link, it is a protocol question for [the sessions page](sessions.md), not something the
relay papers over.

## Build order

1. **Write the threat model** into [`design.md`](design.md) and end the "no remote mode"
   statements in `features.md` and `service.md` in the same change that makes it untrue.
2. **A byte relay over TCP+TLS** with mutual TLS and pinned keys, `serve` and `connect`, and
   an end-to-end test over real sockets with a scripted model, in the style of
   `crates/nanus-link/tests/link.rs`.
3. **Enrolment and key handling**, with the permissions tests the secret store has.
4. **Edge limits.**
5. **A second tunnel** (QUIC or SSH) only if a measured need appears, behind its feature.

Step 2 talks to whatever local endpoint the platform has. Both Unix sockets and Windows
named pipes are implemented; remote access still waits on the relay work described above.

## Tests

- A turn driven through both relays, asserting the file on disk changed, not only that
  frames arrived.
- A client whose key is not enrolled is refused, and so is a gateway whose key changed.
- A connection that drops mid-turn: the turn finishes, and a re-attach sees the result.
- A line longer than the gateway's cap is refused without being buffered.
- A gateway configured to listen publicly with no authentication refuses to start.
- A byte stream that is not this protocol is relayed unchanged and refused by the agent,
  proving the relay did not start parsing.

## Open questions

- Is a single enrolled key per host enough, or does each person need their own, with the
  gateway recording whose connection it was?
- `nanus-cli` or a separate `nanus-remote` binary? A separate one keeps the TLS dependency
  out of the core binary, and the separate-binary precedent is `nanus-tui`.
- Which tunnel first? SSH needs no code and may be enough for the first users.
- Should the gateway let a remote peer set `all_calls`, or refuse the permissive approval
  states from the network? "Fully equivalent" says no restriction; the fail-closed default
  in [`design.md`](design.md) argues for asking.
- Does a remote client need a replay window larger than the agent gives on attach?
