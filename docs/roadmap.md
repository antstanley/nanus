# Roadmap

What is planned, in the order it is worth doing, with a rough effort attached.
This is a plan rather than a promise: the sizes are relative guesses by someone
who knows the codebase, not dates, and nothing here has an owner. Where an
"obvious" feature is deliberately absent, it is in
[by design, not planned](#by-design-not-planned) with the reason, because the
[design decisions](design.md) that explain why are usually the interesting part.

Every item in [what is not supported yet](features.md#not-supported-yet) appears
here. A few more come from the limits recorded in
[status](status.md#known-limits), [sessions](sessions.md#known-limits),
[service](service.md#known-limits), and [the interface](tui.md#what-is-deliberately-missing).

## How to read the sizes

| Size | Rough shape |
|---|---|
| **XS** | A focused change in one place — an afternoon at most. |
| **S** | About a day: one crate, its tests, and a doc line or two. |
| **M** | Several days: a new module or a small protocol change, plus integration tests. |
| **L** | A week or more: a new crate or a cross-cutting change that wants a design note first. |
| **XL** | Architectural: it changes a load-bearing invariant or adds a platform. |

Sizes assume one person familiar with the repository. They are not additive, and
an item that depends on another is noted — doing the dependency first usually
makes the dependent cheaper.

## Now: close the gap between what is written and what runs

These are first because they are correctness, not new surface. Two of them are
places where a document already says something the code does not yet do.

| # | Item | Size | Notes |
|---|---|---|---|
| 1 | **Enforce approval at the tool boundary.** | **M** | `ApprovalPolicy`, `ApprovalRequest`, and `ApprovalOutcome` exist and `ask` / `never` parse, but nothing constructs a request or gates on a decision: `AgentRunner::run_tools` executes every call directly. `Never` should deterministically deny and `Ask` should deny when no answerer exists, exactly as the fail-closed types describe. The default system prompt should also state the runtime policy — the domain's `runtime_context` renders it, but the bundle never calls it. |
| 2 | **Interactive consent in the interface.** | **L** | Depends on 1. New link frames for a request and its answer, a dialog in the interface, and an answerer in the CLI, so `ask` means something when a person is watching. Until both land, [sandbox mode](features.md#safety-and-verification) is the only enforcement. |
| 3 | **Run a step's tool calls concurrently, bounded by `max_parallel_tools`.** | **M** | The setting is validated, plumbed into `AgentConfig`, and printed by `nanus config`, but `run_tools` awaits each call in turn. The work is cooperative concurrency on the single-threaded runtime with ordered event recording (a call and its result must still pair up in the log) and tests for out-of-order completion. |
| 4 | **Verify live tool-call frames against the real API.** | **S** | `live_wire.rs` replays a documented shape rather than a captured one. Record a real tool-call trace and replay it; if it differs, [`wire.rs`](../crates/nanus-adapter-deepseek/src/wire.rs) is the only file to change. This is verification, not a feature. |
| 5 | **Version the link handshake.** | **S** | Today the two binaries ship together and the core never searches `PATH`, so a mismatch is a crash to diagnose. A version field in the handshake turns it into a sentence naming the mismatch. |
| 6 | **Delete a session from the CLI.** | **S** | `StorePort::delete` exists and `nanus sessions` does not expose it. `nanus sessions delete <ref>` with the same refusal rules as naming; a confirmation in the interface can follow. |

## Next: what the interface needs to be a daily driver

| # | Item | Size | Notes |
|---|---|---|---|
| 7 | **Syntax highlighting in fenced code blocks.** | **M** | Deliberately absent today; fences draw verbatim in the code style. A highlighter dependency and a theme that inherits the monochrome story, kept inside the view so it cannot do I/O. |
| 8 | **A help overlay for the key list (`?`).** | **XS** | The [key table](tui.md#keys) is the list; this draws it, gated so `?` is still a `?` in a prompt. |
| 9 | **More slash commands.** | **S** each | Only `/exit`, `/quit`, and `/stats` exist. `/help`, `/clear`, and eventually `/model` and `/compact` are the obvious next ones; an unrecognised command is already named rather than sent to the model. |
| 10 | **Switch model at runtime (`Alt+P`).** | **M** | A config-and-restart decision today. Needs the request to carry the switch and a rule for whether it persists; depends on the provider seam. |
| 11 | **Extended-thinking toggle (`Alt+T`).** | **S** | Needs the same request plumbing as 10. `reasoning_effort` is already a per-request control. |
| 12 | **Permission-mode switching (`Shift+Tab`).** | **M** | Depends on 2; a dialog to move between the presets `PermissionPreset` already defines. |
| 13 | **Paste an image (`Ctrl+V`).** | **M** | Needs clipboard access this program does not have, plus a way to send bytes that are not a file path — `read_image` proves the wire side works, but it takes a path. |
| 14 | **`@` file mentions.** | **M** | A completion over workspace paths and a prompt expansion, with the same rooted-filesystem rule the tools use. |
| 15 | **`!` bash mode.** | **M** | A direct shell escape that does not go through the model. It is a convenience with a sharp edge, so it wants a safety note before it wants code. |
| 16 | **Vim mode.** | **L** | An input *mode* rather than a shortcut: a modal layer over the composer, and a feature in its own right. |

## Next: sessions and the link

| # | Item | Size | Notes |
|---|---|---|---|
| 17 | **Let a client attaching mid-turn catch up.** | **M** | A client that joins late sees the rest of the turn and no more. The transcript is already in the store, so this is a decision about seeding the view from the log rather than a new source of truth. |
| 18 | **Propagate a rename to a held session.** | **S** | The store updates immediately and resolving works; a *listing* of held sessions can show the old name until the agent next opens it. |
| 19 | **Lock a session, or refuse a second writer.** | **M** | Two agents can resume one conversation and the second save wins. A lock file or a store-level check with a clear error, keeping attaching to a live session as the supported path. |
| 20 | **Decide names: namespaces and case.** | **S** | Names are flat and case-sensitive today, so `Nightly` and `nightly` are two names. Small to change; mostly a decision. |
| 21 | **Manage the context window.** | **L** | Not in [features.md](features.md), but implied by it: prompt assembly replays the whole log, so a long session eventually exceeds the model's window. A summarise-or-drop policy, deterministic and tested, because a silent truncation is worse than a refusal. |

## Next: new capabilities

Three additions larger than a feature but short of a rewrite: a way to package
behaviour, a way to hold a secret, and a goal that outlives a turn. Skills come
first because `/goal` is a natural thing to ship as one.

| # | Item | Size | Notes |
|---|---|---|---|
| 22 | **Agent skills: `SKILL.md` discovery and progressive disclosure.** | **M** | A skill is a directory with a markdown file whose frontmatter names it and says when to use it; the body is loaded only when it applies. PrimeIntellect's [goal skill](https://github.com/PrimeIntellect-ai/prime-agent/blob/b6ac5d014d99401b55820835a4966584271e9a3c/packages/coding-agent/skills/goal/SKILL.md) is the reference shape. The pieces are a loader (a user directory under `<nanus home>/skills/`, optionally a workspace one), a frontmatter parser, and a way to reach the body: either a `skill` tool — which collides with the seven-tool invariant and would need a design note — or a prompt section, which is where the domain's unused `PromptBuilder` already points. A skill read from the workspace is untrusted input, as [SAFETY.md](../SAFETY.md) says of anything that reads files. Grows to **L** if a packaged core library and a tool are both wanted. |
| 23 | **Secret storage: an OS keychain instead of `DEEPSEEK_API_KEY`.** | **L** | Move the provider key out of the environment and into the platform store — Keychain on macOS, Secret Service on Linux, Credential Manager on Windows — behind a new `SecretPort` in `nanus-ports` and a keyring adapter, with `nanus auth set` / `clear` / `status` and the `nanus config` presence line it already prints. The environment variable stays as a fallback for CI and containers. The hard part is the service: a detached `nanus service` may run with no unlocked keychain and no session bus, so the design needs a defined fallback (a `0600` file under `NANUS_HOME`) or a fail-closed refusal, and the key must never reach the config, a log, or `Debug` — guarantees the current design already keeps. |
| 24 | **A goal: a durable objective that continues across turns (`/goal`).** | **L** | A session-scoped completion contract: one objective per session, persisted in the log, with a phase (`active` / `paused` / `blocked` / `complete`), a budget, and evidence-based completion. Three surfaces, which can land in stages: the state as a durable `goal/change` session event folded by the domain (the log is already the only source of model history); model tools to read, edit, and complete it; and a human `/goal` that needs a new link request, because the interface cannot mutate a session it does not own. Automatic continuation is an agent-side driver that queues one turn while the session is idle, bounded by the goal budget *and* the existing turn budget. See [the goal research note](goal-research.md). The largest of the three and the one with the most decisions left open; possibly **XL** if it lands whole. |

## Later: larger bets

| # | Item | Size | Notes |
|---|---|---|---|
| 25 | **A second model provider.** | **M** | The `LlmPort` seam is real and an OpenAI-compatible adapter is mostly request and response encoding; a genuinely different protocol is more like an **L**. Nothing in the tools or the domain should change. |
| 26 | **An OS-enforced sandbox.** | **XL** | Confinement is advisory: the tools refuse or confine writes, but an approved program can do anything the user can, including reach the network. Landlock or seccomp on Linux and `sandbox-exec` on macOS, with the platform and `unsafe` story written down first. |
| 27 | **Network confinement.** | **XL** | Part of 26, and separable only if 26 lands as a mechanism with more than one policy. |
| 28 | **Windows support.** | **XL** | The link is a Unix domain socket and the shell adapter depends on `nix` for process groups, so this is a transport plus a process-lifecycle story, not a build flag. |
| 29 | **Automated end-to-end tests for the interface.** | **M** | Raw mode needs a real terminal; a PTY-backed test binary would cover the alternate screen and the drawing that are manual today. |
| 30 | **Background tasks (`Ctrl+B`).** | **L** | Needs a task model the agent and the session own, not just a key in the interface. |

## By design, not planned

These appear on other projects' roadmaps. They are absent here on purpose, and
the [design decisions](design.md) say why:

- **An auto-approve or "always allow" policy.** A mode that means yes to
  everything is how an `rm -rf` reaches a bug report. The policy set is `ask`
  and `never` and stays that way.
- **Aliases for retired model ids.** `deepseek-chat` and `deepseek-reasoner` do
  not resolve, because silently mapping a retired name onto a new model changes
  a user's output without saying so.
- **A remote link.** The socket is local, unauthenticated, and trusts processes
  running as the user; there is no remote mode to secure, and
  [SAFETY.md](../SAFETY.md) asks that one not be added by forwarding the socket.
- **An in-process interface.** The core not linking the interface is the
  structural decision that keeps `nanus run` small; the socket is the price.
- **A larger core toolset.** Seven tools is the design. A tool earns its place
  by being a mechanism the shell cannot provide as well, and convenience
  wrappers are not it.
- **The API key in the configuration file.** It is read from the environment on
  use and has no field to live in.
- **An image fetch or a URL request from the markdown renderer.** The view is a
  pure function of the transcript; a remote fetch on a model's say-so is a
  request the reader did not ask for.
- **A worker pool or parallel turns.** The kernel is single-threaded and its
  futures are deliberately `!Send`; concurrency is cooperative, and item 3 is
  the extent of the parallelism planned.
