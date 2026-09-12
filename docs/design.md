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

### Approval is fail-closed, and there is no "auto"

`ApprovalPolicy` is `Ask | Never`. `ApprovalOutcome` is
`AllowedOnce | Rejected | Cancelled | Unavailable`. Anything other than `AllowedOnce`
denies — a harness that cannot obtain an answer denies rather than proceeding.

`Never` deterministically rejects *without consulting anyone*, so a later-registered
policy cannot bypass it. There is deliberately no auto-approve mode, because a mode
that means "yes to everything" is a mode that ends with someone's `rm -rf` in a bug
report.

### A budget, because unattended loops are a cost hazard

The reference harness has **no** step budget: a tool loop continues until the model
stops calling tools. `nanus` bounds a turn. This is a deliberate divergence and is
documented at the type that enforces it.

### The current model ids, and no aliases for the dead ones

`deepseek-chat` and `deepseek-reasoner` were discontinued on **2026-07-24**. The
supported ids are `deepseek-flash` and `deepseek-v4-pro`, at
`https://api.deepseek.com` with no `/v1`.

Retired ids are deliberately **not** offered as aliases. Silently mapping a retired name
onto a new model would change a user's output without telling them, and a test asserts
the retired names do not resolve.
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

Together they are why "everything is a plugin" is a mechanism here rather than a slogan.
The agent loop itself is a plugin. So you can replace it.

The design follows _A Programming Paradigm for Spatiotemporal Composability_
([arXiv:2608.25512](https://arxiv.org/abs/2608.25512)). Where the paper and the shipped
framework disagree, [`cordis-mechanisms-report.md`](../cordis-mechanisms-report.md)
records which one this kernel follows and why — the paper describes an idealised runtime,
and the TypeScript implementation diverges from it in ten documented places.
