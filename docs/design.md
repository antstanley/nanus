# Design decisions

Every choice below is a deliberate reading of the reference implementation, or a
deliberate divergence from it. They are written down because a reader comparing `nanus`
with [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) will otherwise
assume the differences are mistakes.

Most of them are divergences *toward* a guarantee: something the reference leaves to
convention, `nanus` makes a property of the types.

### Safe Rust, because the model is writing the code

`unsafe` appears **nowhere** in this repository. Every crate carries
`#![forbid(unsafe_code)]` *and* the workspace sets the lint — because the manifest lint
alone does not cover doctests, and a guarantee with an asterisk is not a guarantee.

This matters more than it sounds. A harness asks a language model to produce code that
will be compiled and run. The one thing you cannot afford is a memory-safety bug in the
layer that decides what to run.

The shell adapter needs to kill a whole process group. Without `unsafe`, that means
[`nix`](https://crates.io/crates/nix)'s safe wrappers rather than `libc` — which is
also, it turns out, the correct call for reasons that have nothing to do with safety
(see below).

### A shell tool that reaps its grandchildren

An agent harness runs commands as `sh -c "…"`, which means the process doing the work
is a **grandchild**. `kill_on_drop(true)` and `Child::kill()` do not reap
grandchildren. A killed tool leaves a process running behind it.

This was measured, not assumed: the experiment is reproducible and the finding is
recorded in the research repository. `nanus` spawns with `process_group(0)` and kills
the group. There is a test that reproduces the orphan and asserts it is gone.

A related trap found the same way: reading a subprocess pipe with
`AsyncReadExt::take(n)` **hangs** rather than truncating, because the writer blocks once
the 64 KiB pipe buffer fills. Output is read to end-of-file into a bounded buffer and
truncated after.

### Seven tools, and the count is the design

`read` · `write` · `edit` · `read_image` · `glob` · `grep` · `bash`

Each one is a mechanism a shell *cannot* provide as well — a bounded read window, an
exactly-once edit, a unified diff, a capped search that reports when it hit the cap.
Not one of them is a convenience wrapper around something `sh` already does.

There is deliberately no `list_directory`, no `move_file`, no `make_directory`, and no
per-language tool. Every tool costs a description in every request and a schema the
model must choose between. A tool earns its place by being irreplaceable, not by being
convenient.

### Only three fields of a tool can reach the model

A tool's `name`, `description`, and `parameters` may be serialised into a request. Its
executor, timeout, and presenters may not.

This is not a convention, it is a property of the types: the executable half is *not*
`Serialize`, so there is no code path by which a filesystem root, a sandbox policy, or a
session id can be encoded into a request body. The allowlist is carried by the compiler,
and a test asserts the serialised key set is exactly those three.

### Approval is a three-state axis, fail-closed at the default

`ApprovalPolicy` is `PerCall | Permitted | AllCalls`. `ApprovalOutcome` is
`AllowedOnce | Rejected | Cancelled | Unavailable`. Anything other than `AllowedOnce`
denies — a harness that cannot obtain an answer denies rather than proceeding.

`PerCall` is the default and asks about every exception; with no answerer it is a denial,
so an unattended run still fails closed. `Permitted` grants the exceptions that cannot
destroy anything and asks about the rest, except that a destructive call whose targets are
all inside a temporary directory is granted too — that is where a destructive command is
the ordinary way to clean up. `AllCalls` grants every exception without asking, and it is
for an environment that enforces its own containment: a container, a virtual machine, a
machine whose contents are disposable.

The two permissive states are explicit names a human has to choose, and the default is the
restrictive one, which is the part of "fail closed" that still holds. `AllCalls` is the
state this design used to rule out, and it exists because the honest alternative was not
"safer": an operator who wants a free-for-all environment arranges one, and a harness that
refuses to name the state only hides where it was chosen. Nothing about it is silent — the
status line always says which state is in force, and Shift+Tab opens the dialog that chooses
between them.

An answer may also be *standing*: the interface's "always allow" records the tool for the
session, so the same question is not asked again for the rest of that conversation. The
record is per session and in memory, so nothing about it is written to the log or outlives
the agent.

### A budget, because unattended loops are a cost hazard

The reference harness has **no** step budget: a tool loop continues until the model
stops calling tools. `nanus` bounds a turn. This is a deliberate divergence and is
documented at the type that enforces it.

The bound is 512 steps. It began at sixteen, on the theory that a genuine tool-using turn
is a handful of steps and anything longer is a runaway; the first multi-file task this
harness was given — three counters in the interface, touching two crates — spent
twenty-seven of its thirty-two steps reading before it made its first edit and closed
mid-change. A bound a normal task hits is not bounding a runaway; it is bounding the task.
So it went to a hundred and twenty-eight, and then to five hundred and twelve: the last
raise is headroom for the work that legitimately takes a long time — a refactor across
several crates, a task whose build-and-test cycle runs a dozen times — rather than a fix
for a failure anyone watched happen, and the honesty of the number is that a runaway loop
is now stopped later than a long turn ends.

Two things came out of the first raise besides the number. The model is told its budget in
the system prompt, because a ceiling nobody mentioned is not one it can pace against. And
the ending carries the reason it stopped, because a turn cut off at the budget used to
reach the interface looking exactly like one that had finished.

### The current model ids, and no aliases for the dead ones

`deepseek-chat` and `deepseek-reasoner` were discontinued on **2026-07-24**. The
supported ids are `deepseek-flash` and `deepseek-v4-pro`, at
`https://api.deepseek.com` with no `/v1`.

Retired ids are deliberately **not** offered as aliases. Silently mapping a retired name
onto a new model would change a user's output without telling them, and a test asserts
the retired names do not resolve.

### One agent, three lifetimes

The agent is the same object whether it is answering one prompt, serving an interface, or
running as a service. What differs is how long it lives and how it is reached — and the
reach is the same in all three cases, because the interface is always a client. That is
what keeps the modes from becoming three agent implementations that agree until they do
not: there is one code path that runs a turn for someone to watch.

| Mode | Lifetime | Reach |
|---|---|---|
| `run` | one turn | stdout, in the same process |
| `tui` | the interface's | a socket, for a local task |
| `service` | until stopped | the same socket, for a process |

The service is not a special case of the interface, and the interface is not a special
case of the one-shot run. They are three answers to "how long", on top of one answer to
"what".

### A session belongs to the agent, not to the connection

The first version made a connection *be* a conversation: connect, and you had a session.
It was pleasant and it made the lifetimes fall out — close the interface, close the agent —
but it also meant a conversation could not be reached twice. Resuming was reading, and a
session an agent was still holding was unreachable, because nothing could name it.

Now the agent owns sessions and a connection is a view of one. A client says `new` or
`attach`, the agent keeps holding the session after that client leaves, and a turn runs in
its own task so it outlives the terminal that asked for it. The costs are real: one turn at
a time per session, a client that attaches mid-turn missing the frames already sent, and a
registry that has to bound itself. The benefit is that a conversation is a thing rather
than an event, which is what makes naming, resuming, and watching the same feature.

Sessions are also deliberately *not* streamed over the link. A client that wants the
conversation reads it from the store, where it is already durable, rather than receiving a
second copy that would make the socket a second source of truth. The link carries what
happened, not what was.

### The interface is a program, not a library

The core does not link the interface, and that is enforced by the manifest rather than by
intention: `nanus-cli` has no dependency on `nanus-tui`, and could not call into it if it
wanted to. The cost is a protocol and a socket. The benefit is that the part which grows —
the interface — grows in its own address space, with its own dependency set, at its own
rate, while the part a boot script starts and a person audits stays small enough to read.

The alternative was measured before it was rejected: an earlier revision had one binary
that did both, which is genuinely simpler, and it put `ratatui`, `crossterm`, and the
whole view layer in the dependency set of `nanus run`. That trade is worth making once,
deliberately, rather than discovering it later as "why is our CLI 40 MB".

The socket is a [local link](../docs/tui.md#the-link): one frame per line of JSON over a
Unix domain socket, `0600` inside a `0700` directory. There is no safe *in-process* channel
between two processes — sharing memory across a `fork` needs `mmap` and `unsafe`, and this
workspace forbids `unsafe` everywhere — so a domain socket is what "in memory" reduces to
when the two ends are two programs.
## The two halves, and why they are the mechanism

The kernel provides *spatiotemporal composability*. The two words are worth unpacking,
because they name the actual mechanism rather than a mood:

- **Temporal** — every mutation a component makes is recorded *with its inverse*.
  Unloading the component reverts those effects in reverse order, so services withdraw,
  listeners unregister, and consumers deactivate. There is no partial teardown and no
  stale registration. It is tested by asserting the revert order, not by inspection.
- **Spatial** — a component declares the services it needs (its *coeffects*), and the
  runtime activates it when they appear and deactivates it when they vanish. Load order
  is a dependency, never a boot script. The tool provider and the model adapter can be
  staged in either order.

Together they are why "everything below the loop is a plugin" is a mechanism here rather
than a slogan. The clock, the filesystem, the shell, the session log, the model adapter, the
credential stores, and the tool registry are services on the kernel, and the loop consumes them, so replacing the
provider or the toolset needs no edit to the loop at all.

The loop is not itself a plugin, and this page used to say it was. It is built — by
`compose::build_runner`, over those same handles, which is what makes the sentence above
true — and then handed to the caller rather than provided as a service, so replacing it
means editing the composition rather than unloading one component and mounting another.
Closing that gap is the one place the framework's story is still a promise rather than a
mechanism, and it is worth doing for the reason the promise was made: a replacement loop
registered like any other component, activated when the ports it needs appear.

The design follows _A Programming Paradigm for Spatiotemporal Composability_
([arXiv:2608.25512](https://arxiv.org/abs/2608.25512)). Where the paper and the shipped
framework disagree, [`cordis-mechanisms-report.md`](../cordis-mechanisms-report.md)
records which one this kernel follows and why — the paper describes an idealised runtime,
and the TypeScript implementation diverges from it in ten documented places.
