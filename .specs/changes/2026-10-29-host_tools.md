# Change: Host tools — a supported registration hook and declared destructiveness

**Status:** Implemented locally (unpublished) · **Date:** 2026-10-29 · **Owner:** Ant Stanley · **Target:** `nanus-domain`, `nanus-bundle`; embedding hosts

Give an embedding host a supported way to register a tool of its own, and make the `permitted`
approval state ask about such a tool unless the tool says it cannot destroy anything. Nothing
about the stock seven, the goal tools, the wire, the session format or the link protocol changes.
The guide for an author is [`docs/host-tools.md`](../../docs/host-tools.md).

## Motivation

A host that links `nanus-bundle` could already put a tool in front of the model. The tool types
are public, and `Harness::runner.tools()` (or the published `tools` service) hands out the
registry, which the runner reads on every request. `read_video` is installed exactly that way.
Two things kept this from being a contract a host could rely on:

1. **No supported moment to register.** A tool registered after `Pending::start` returns
   reaches the model, but the link handshake has already advertised the tool count by then
   (`nanus-link/src/server.rs`, `harness.tool_count()`), and a tool that appears mid-session
   changes the cached request prefix. Nothing refused a goal-tool or context-tool name, either:
   the loop answers those itself, so such a tool was dropped from the request with a warning and
   never ran.
2. **`permitted` ran an unknown tool without asking.** `guard::is_destructive` read a call's
   `command` argument, whatever the tool was. A host's tool with no `command` argument was
   therefore "not destructive", and under `permitted` a call the sandbox did not allow ran
   without anyone being asked: a tool that sent a message, submitted a form or deleted a record
   included. The guard's own comment said a deleting tool "must be named here", which a host
   outside this repository cannot do.

## Affected spec pages

| Canonical contract | Change |
|---|---|
| [Design → Approval](../../docs/design.md#approval-is-a-three-state-axis-fail-closed-at-the-default) | `permitted` asks the tool's declaration, and undeclared is destructive. |
| [Design → Seven tools](../../docs/design.md#seven-tools-and-the-count-is-the-design) | A host adds tools beside the seven through the hook, under the same bar. |
| [Features → The agent; Safety and verification](../../docs/features.md#the-agent) | Name the hook and the declaration. |
| [Safety → Defaults](../../SAFETY.md#defaults-and-what-they-do-not-protect-you-from) | `permitted` always asks about an undeclared host tool. |
| [Host tools](../../docs/host-tools.md) (Add) | The author's guide. |
| [AGENTS.md → Invariants; How to make common changes](../../AGENTS.md) | Two invariants and a how-to. |
| [Library embedding spec, N2](2026-09-30-library_embedding_and_multimodal_results.md#n2-design-decisions--approval-is-a-three-state-axis-fail-closed-at-the-default-modify) | Its "absence of a policy preserves today's default gate exactly" no longer holds for an undeclared tool under `permitted`; dated note added there. |

Rust contracts are canonical. No wire field, JSON Schema, session-version or protocol-version change.

## Changes

### C1. `Destructive`, declared by a tool (`nanus-domain`)

```rust
pub enum Destructive { Never, ByCommand, #[default] Always }

impl ToolDefinition {
    pub const fn with_destructive(self, destructive: Destructive) -> Self;
    pub const fn destructive(&self) -> Destructive;
}
```

Beside `ToolAccess` in `approval.rs`, re-exported from the crate root, `serde` as `never` /
`by_command` / `always`. It is local metadata like `ToolAccess`, not part of `ToolSchema`, so the
wire allowlist is untouched. `ToolDefinition::new` defaults it to `Always`.

### C2. The gate reads the declaration (`nanus-bundle`)

`AgentRunner::gate` copies `(access, destructive)` out of the resolved definition before awaiting
anything, as it already did for access. `exempt` and the approval reason use two new functions in
`guard.rs`:

| Declaration | `call_is_destructive` | `permitted_runs` |
|---|---|---|
| `Never` | `false` | `true` |
| `ByCommand` | `is_destructive(call)` | `!is_destructive(call) \|\| targets_are_temporary(call)` |
| `Always` | `true` | `false` |

The temporary-directory exemption applies to `ByCommand` only, because it reads paths out of a
command line. `is_destructive` and `targets_are_temporary` are unchanged and still public; they are
now documented as the classifier for a `ByCommand` tool. `per_call`, `all_calls`, the sandbox
matrix and the host `ToolPolicy` are unchanged.

Stock declarations: `read`, `read_image`, `glob`, `grep`, `write`, `edit` → `Never`; `bash` →
`ByCommand`; `read_video` → `Never` (it copies, probes and samples one file and deletes nothing of
the caller's). Every stock tool therefore behaves exactly as before.

### C3. The checked registration (`nanus-bundle::host_tools`)

Available with and without `stock-compose`:

```rust
pub fn is_reserved(name: &ToolName) -> bool;   // goal tools and context tools
pub fn register(registry: &mut ToolRegistry, definition: ToolDefinition) -> Result<(), BundleError>;
```

`register` refuses a reserved name and a taken name with `BundleError::Config` naming the tool,
and registers nothing in either case.

### C4. `Pending::with_tools` (`stock-compose`)

```rust
impl Pending {
    pub fn with_tools(self, install: impl FnOnce(&mut HostTools<'_>) -> Result<(), BundleError> + 'static) -> Self;
}
pub type ToolInstaller = Box<dyn FnOnce(&mut HostTools<'_>) -> Result<(), BundleError>>;
```

`start` runs the queued installers in order after the stock seven and `read_video` are registered
and the `ProviderSwitch` exists, and before the `Harness` is returned. The first failure fails
`start`. `HostTools` offers `register` (C3), `contains`, and the composition's own `fs()`, `shell()`,
`clock()`, `workspace()` and a `Weak<ProviderSwitch>` (weak because the switch owns the runner,
which owns the registry, which owns the tool). The registry is borrowed only inside `register`, so an
installer can build a tool that asks the runner something.

## Compatibility

- **Source-compatible.** Every change is additive: a new enum, two builder/accessor methods, a new
  module, a new `Pending` method, a new private `Pending` field. Existing calls to
  `ToolDefinition::new`, `with_access`, `ToolRegistry::register`, `guard::is_destructive` and
  `Pending::start` compile and behave as before. `BundleError` is `#[non_exhaustive]` and gains no
  variant.
- **Behaviour.** One change, chosen deliberately by the owner on 2026-10-29 over keeping the old
  rule: a host's tool that declares nothing, whose access the sandbox does not permit, is now
  **asked about under `permitted`** where it previously ran. `per_call` (the default) and
  `all_calls` are unaffected, and so is any `Read` tool, because every sandbox mode permits reads.
  A host restores the old behaviour for a tool by declaring `Destructive::Never`; one whose
  `command` argument is a shell command declares `ByCommand`.
- **Downstream.** The one known embedding host registers no host tools at its pinned revision, so
  nothing it runs changes.

## Acceptance criteria

1. An undeclared tool is `Always`; each declaration round-trips its name (`approval.rs`).
2. `permitted` asks about an undeclared host tool and runs one declared `Never`; a `/tmp` path does
   not exempt an `Always` tool; `Never` does not bypass `per_call`; the four stock `bash` cases
   still hold with the stand-in declared `ByCommand` (`agent_loop.rs`).
3. The declaration table above holds for each declaration and kind of command (`guard.rs`).
4. Every stock tool declares `Never`, except `bash`, which declares `ByCommand` (`lib.rs`); `read_video`
   declares `Never` when FFmpeg is present (`tests/composition.rs`).
5. `register` refuses every goal and context tool name and a taken name, registering nothing
   (`host_tools.rs`).
6. Two installers run in order inside `start`, see the stock tools and the live switch and the
   composition's workspace, and their tools are counted (14) and published; an installer cannot
   take `bash` or `create_goal`, and an installer's own error fails `start` (`tests/composition.rs`).
7. The embedder-boundary approval test covers both declarations under all three policies
   (`tests/embedding.rs`, also run downstream by `examples/embedded`).
8. A downstream crate with `default-features = false` writes a tool, registers it with
   `host_tools::register`, and a scripted model reaches it: the effect lands, a bad argument is a
   failure naming the field, a read-only sandbox with no answerer denies, and a reserved name is
   refused (`examples/embedded/tests/host_tool.rs`).

## Verification (2026-10-29, macOS, local)

- `cargo fmt --all --check`: clean. `cargo clippy --workspace --all-targets --all-features`: no
  warnings. `cargo clippy -p nanus-bundle --no-default-features --all-targets`: no warnings.
- `cargo nextest run --workspace --all-features`: 2364 passed, 30 skipped.
- `cargo nextest run -p nanus-bundle --no-default-features`: 316 passed.
- `cargo test --workspace --doc`: passed; `Pending::with_tools`'s example is `no_run` and compiles.
- `examples/embedded`, `--locked`: default features pass (29 shared embedding cases, 4 host-tool
  cases); with `providers` pass (32, 4, 3).

- `cargo clippy -p nanus-tui --no-default-features --all-targets`: no warnings;
  `cargo test -p nanus-tui --no-default-features`: 398 passed.
- `cargo test -p nanus-bundle --no-default-features --doc`: 2 passed.

Not run: native Windows. No platform-specific code changed.

## Assumptions and open questions

**Assumptions**

- A tool's author is the right party to say whether its calls can destroy something, as for access.

**Decisions**

- _Undeclared default._ **`Always` (fail closed).** Owner decision, 2026-10-29, over keeping the
  command-only rule for undeclared tools.
- _Hook shape._ **A queued `FnOnce` run inside `start`,** rather than registering after `start`
  returns, so the count and the first request agree.

**Open questions**

- `with_tool_policy` is not reachable on the stock composition, because `start` builds the runner.
  A `Pending::with_tool_policy` would follow the same pattern if a stock-composition host needs
  exact-call checks.
- MCP servers and agent skills (roadmap item 22) can build on C3/C4; neither is specified here.
