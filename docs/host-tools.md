# Host tools

How to write a tool of your own and hand it to an agent you embed. This page is for the
author of an application that links `nanus-bundle` as a library, and for the agent writing
that tool on their behalf. It covers what a tool is made of, the two ways to register one,
the two declarations that decide when a person is asked, and the rules a tool has to keep.

The worked example is [`examples/embedded/tests/host_tool.rs`](../examples/embedded/tests/host_tool.rs):
a complete tool, registered from outside this workspace and driven through a scripted model,
kept as a test so this page cannot drift from what compiles. Read it beside this page.

## Should this be a tool at all?

Every registered tool costs its name, description and parameter schema in **every request**,
and it is one more choice the model has to make on every step. The stock seven are about 1,200
estimated tokens together ([measured](features.md#what-a-request-costs-before-the-conversation)).
[The design](design.md#seven-tools-and-the-count-is-the-design) holds the stock set to
mechanisms the shell cannot provide as well, and a host tool should meet the same bar:

- **Good fits** are capabilities only the host has: a browser it drives, a document it holds
  open, an account it is signed in to, a device it talks to, state that lives in its memory.
- **Poor fits** are things `bash` already does — running a CLI, reading a file, calling `curl`.
  A wrapper adds a schema to every request and takes nothing off the model's plate.

If what you want to give the model is *instructions* rather than a mechanism, it is prompt
text, not a tool: the system prompt is a plain string the host supplies.

## The two ways to embed

| | Stock composition | A runner you build |
|---|---|---|
| Dependency | `nanus-bundle` (default features) | `nanus-bundle` with `default-features = false` |
| What you get | The stock providers, credential store, session store, rooted filesystem, process-group shell, the stock seven tools, the link server | The agent loop and the stock tool factories; you supply the model, clock, registry and everything else |
| Where a tool goes in | `Pending::with_tools` | `nanus_bundle::host_tools::register` on your own `ToolRegistry` |
| Exact-call host policy | Not available | `AgentRunner::with_tool_policy` |

Both use the same tool types from `nanus-domain`, so a tool written for one works in the other.

### Stock composition: `Pending::with_tools`

`compose(&config).await` builds the adapters, and `Pending::start()` mounts the kernel and
returns the `Harness` (they are split because of the [staging rule](../AGENTS.md#critical-gotcha-asyncsync-composition-staging):
`start` must run outside the async runtime). Queue an installer between the two:

```rust
use nanus_bundle::compose::compose;

let pending = nanus_kernel::runtime::block_on(compose(&config))?
    .with_tools(|host| host.register(my_tool(host.fs())))
    .with_tools(|host| host.register(another_tool(host.switch())));
let harness = pending.start()?;
```

The installer runs inside `start`, after the stock seven and any configured optional tool
(`read_video`) are registered, and before the harness is returned. That timing matters, and it is
why there is a hook rather than "register through `harness.runner.tools()` afterwards":

- **The link handshake advertises the tool count** when an agent is built over the harness. A tool
  added afterwards still reaches the model, but the interface shows the wrong number.
- **The tool schemas are part of the request's cached prefix.** A tool that appears mid-session
  changes every following request's prefix and invalidates the provider's cache.

Installers run in the order they were queued. A failed installer fails `start`: a harness that
quietly lacked a tool its host asked for would offer the model less than the host promised.
Compose again (a new generation) and the installers have to be queued again, because they are
consumed by `start`.

The installer is handed a `HostTools`, which gives you the composition's own handles:

| Method | What it is | Use it for |
|---|---|---|
| `register(definition)` | The checked registration (below) | Adding a tool |
| `contains(name)` | Whether a name is taken | Optional tools that defer to one already present |
| `fs()` | The rooted filesystem (`FsHandle`) | Reading and writing inside the workspace under the same rules as `read`/`write` |
| `shell()` | The process-group shell (`ShellHandle`) | Running a program whose grandchildren are killed with it |
| `clock()` | The clock (`ClockHandle`) | Timestamps that agree with the session's |
| `workspace()` | The workspace root | Resolving a relative path the model gave you |
| `switch()` | `Weak<ProviderSwitch>` | A tool that asks a model something itself, as `read_video` does |

Keep `switch()` **weak inside the tool**. The switch owns the runner, the runner owns the
registry, and the registry owns your tool, so a strong handle is a cycle that keeps the whole
harness alive. Upgrade it per call and report a failed upgrade as the tool's own failure.

### A runner you build: `host_tools::register`

With `default-features = false` there is no composition: you construct the registry and hand it
to `AgentRunner::new` yourself (the crate-level docs of `nanus-bundle` have the minimal runner).
Register through the checked function instead of calling `ToolRegistry::register` directly:

```rust
let mut registry = nanus_domain::ToolRegistry::new();
nanus_bundle::host_tools::register(&mut registry, my_tool(state.clone()))?;
let runner = AgentRunner::new(llm, ToolRegistryHandle::new(registry), prompt, config, clock)?;
```

You can still add the stock tools: `nanus_bundle::tools` exports their factories (`read_tool`,
`bash_tool` and the rest), each over the ports you supply.

### What the checked registration refuses

- **A name already registered.** A host tool never replaces a stock one, or another host's.
- **A name the loop runs itself:** `get_goal`, `create_goal`, `update_goal`, `pause_goal`,
  `abandon_goal`, `context_manage` and `context_recall`. The loop answers those before the
  registry is consulted, so a registered tool by one of those names would be offered to the
  model and never called. `host_tools::is_reserved` answers the question for a name.

Both are `BundleError::Config` naming the tool, and nothing is registered.

## Anatomy of a tool

A tool is a `ToolDefinition`: a wire half the model sees, an executable half it never does, and
two declarations the approval gate reads.

```rust
use nanus_domain::{Destructive, ToolAccess, ToolDefinition, ToolName, ToolSchema};
use serde_json::json;

fn note_tool(notes: Notes) -> ToolDefinition {
    let schema = ToolSchema {
        name: ToolName::new("host_note").expect("a valid tool name"),
        description: "Pin a short note to the host's notebook.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": { "text": { "type": "string", "description": "The note to pin." } },
            "required": ["text"],
            "additionalProperties": false
        }),
    };
    ToolDefinition::new(schema, NoteExecutor { notes })
        .with_access(ToolAccess::Write)          // what a call can reach
        .with_destructive(Destructive::Never)    // whether what it does can be undone
}
```

(`expect` is fine in a test; production code in a crate that denies it should map the error.)

### The schema: the only part the model sees

- **`name`**: `[a-z0-9_-]`, at most 64 bytes (`TOOL_NAME_MAX_LEN`), which is the alphabet every
  provider accepts. **Prefix it with your application's name** (`myapp_search`): it keeps the
  name clear of a stock or future tool, and it tells the model whose tool it is.
- **`description`**: what the tool does and when to reach for it, in a sentence or two, with any
  bound the model can hit (a size limit, a rate, what a failure means). It is in every request, so
  every word costs.
- **`parameters`**: a JSON Schema object. Mark what is required, set `additionalProperties: false`,
  and describe each field. Providers differ on the schema features they accept; plain objects,
  strings, integers, booleans, enums and arrays are safe everywhere.

These three fields are the whole of the [wire allowlist](design.md#only-three-fields-of-a-tool-can-reach-the-model):
the executor and everything it closes over are not `Serialize`, so a handle, a path or a secret
your tool holds cannot be encoded into a request.

### The executor: where the work happens

```rust
use nanus_bundle::args::Arguments;
use nanus_domain::{ToolCall, ToolExecutor, ToolFuture, ToolResult};

struct NoteExecutor { notes: Notes }

impl ToolExecutor for NoteExecutor {
    fn execute(&self, call: ToolCall) -> ToolFuture {
        let notes = Rc::clone(&self.notes);          // move what the future needs into it
        Box::pin(async move {
            let args = Arguments::new(call.name.as_str(), &call.arguments);
            let text = match args.required_str("text") {
                Ok(text) => text,
                Err(outcome) => return ToolResult::new(call.id, outcome),
            };
            notes.borrow_mut().push(text);
            ToolResult::success(call.id, serde_json::json!({ "pinned": true }))
        })
    }
}
```

- **`ToolFuture` is `'static` and need not be `Send`.** The agent runs on one thread, so `Rc`,
  `RefCell` and `!Send` handles are fine. Clone what the call needs into the `async move` block;
  the future cannot borrow `self`.
- **The registry validates before you are called**: the name is yours and the arguments are a JSON
  object. Their *contents* are the model's, so read them with `nanus_bundle::args::Arguments`
  (`required_str`, `optional_str`, `optional_u32`, `flag`). A wrong type or a missing field
  comes back as a `ToolOutcome` failure that names the field, and the model can fix that.
- **A failure is a result, not an error.** `ToolResult::failure(call.id, "…")` is what the model
  reads; write it so the model can act on it. Never panic: a panic in an executor takes the agent
  down, and the crates here deny `panic!`, `unwrap` and `expect` in production code for that reason.
- **Return what the model needs, compactly.** `ToolResult::success(id, json)` renders the value as
  text; `ToolOutcome::success_with(value, blocks)` returns ordered `ContentBlock`s (text, and images
  for a model that takes them). The result stays in the conversation for every following request.

## When a person is asked

Two declarations decide whether a call runs, asks, or is refused. Both default to the most
restrictive value, so a tool that declares nothing is asked about everywhere it can be.

### `ToolAccess`: what a call can reach

The sandbox mode is the standing permission: a call it permits runs without anyone being asked.

| Access | `read_only` | `workspace_write` | `danger_full_access` |
|---|---|---|---|
| `Read` | runs | runs | runs |
| `Write` | needs an exception | runs | runs |
| `Execute` (default) | needs an exception | needs an exception | runs |

Choose by what the call can affect, not by the name of the operation:

- **`Read`** only observes: a lookup, a search, a fetch of the host's own state.
- **`Write`** changes something the user owns that another call could put back: a file, a note, a
  draft, a setting.
- **`Execute`** runs a program, reaches outside the machine, or does something the workspace root
  cannot contain: sending a message, submitting a form, spending money, calling a remote API with
  side effects.

### `Destructive`: whether what it does can be undone

A call the sandbox does not permit needs an exception, and the approval policy decides it:

| | `Never` | `ByCommand` | `Always` (default) |
|---|---|---|---|
| `per_call` | asks | asks | asks |
| `permitted` | **runs** | runs unless the command deletes outside a temporary directory | **asks** |
| `all_calls` | runs | runs | runs |

- **`Never`**: no call removes or irrecoverably overwrites anything. `write` and `edit` are
  declared this way: they replace contents that another call can put back.
- **`ByCommand`**: how `bash` is declared. The harness reads the call's `command` argument
  for a deleting word (`rm`, `dd`, `-delete`, …) and lets a deletion confined to a temporary
  directory through. Use it only for a tool whose `command` argument really is a shell command.
- **`Always`**: a call may destroy something — a delete, a send, a purchase, an overwrite with no
  history. **This is what a tool that declares nothing gets**, so `permitted` asks about it until
  you say otherwise.

A person can grant a tool "always allow" for the rest of a session from the approval dialog; that
is the person's decision, recorded in memory only. The approval request carries the tool's name and
the reason, never its arguments.

### An exact-call policy

A runner you build can add `with_tool_policy(policy)`: a `ToolPolicy` sees every registered call,
with its arguments and declared access, before the sandbox and approval gate, and answers
`UseDefault`, `AllowOnce` or `Deny { reason }`. A policy error denies. Use it when the host knows
something the declarations cannot say, for example that this particular URL is on an allowlist. The
stock composition does not expose it.

## Rules a host tool keeps

1. **Bound your output.** The runner does not truncate a host tool's result. Cap text (the stock
   `bash` keeps 64 KiB) and say in the result that it was capped and how to get the rest.
2. **Bound your time.** A definition has no timeout field. Enforce your own deadline and report it
   as a failure; a call that never returns stalls the turn.
3. **Be cancel-safe.** The turn can be cancelled and your future dropped mid-call. Do not leave
   half-applied state that a later call trips over; detached work (a spawned task, a child process)
   is yours to stop. `HostTools::shell()` gives you a shell that kills a call's whole process group
   (a Job Object on Windows).
4. **Declare images.** A tool that returns images should declare its worst case with
   `with_result_images(max_images, max_bytes_each)`. The runner reserves that against the model's
   image caps before the call runs, and replaces a result that exceeds it with a failure. A tool
   that declares nothing is not admitted against.
5. **Keep secrets in the executor.** Anything in the result is in the conversation, the session
   log on disk, and every later request. A credential the tool uses stays in the executor's
   closure; the wire allowlist already keeps it out of the schema.
6. **Register once, before serving.** Don't add or remove tools during a session (see above).
7. **Treat arguments as untrusted.** They are written by a model that may have read hostile
   content. A path argument can climb out of the workspace unless you resolve it through
   `HostTools::fs()`, which is rooted; a URL can point anywhere.

## Testing a host tool

`examples/embedded/tests/host_tool.rs` is the pattern: a scripted `LlmPort` that calls the tool and
then answers, a real runner, and assertions on the effect in the host's state as well as on the
recorded `ToolResult`. Test both directions, as everything in this repository does:

- the call that works: the tool is offered, called, and its effect lands;
- a bad argument: the model gets a failure naming the field, and nothing changed;
- the gate: under a sandbox that does not permit the access, with nobody to answer, the call is
  denied and does not run;
- the name: a reserved or taken name is refused.

For the stock composition, `crates/nanus-bundle/tests/composition.rs` shows how to compose a real
harness in a test without a network call (the placeholder credential is set in a re-executed child,
because this workspace forbids `unsafe` and therefore `std::env::set_var`).

## Checklist

- [ ] The tool is a mechanism the model cannot get from `bash`.
- [ ] The name is prefixed and matches `[a-z0-9_-]{1,64}`.
- [ ] The description says when to use it and what its bounds are, briefly.
- [ ] The schema marks required fields and sets `additionalProperties: false`.
- [ ] `with_access` is the narrowest class that is true.
- [ ] `with_destructive` is declared; `Never` only if no call can lose anything.
- [ ] Arguments are read with `Arguments`; every failure is a `ToolResult` failure.
- [ ] Output and time are bounded; cancellation leaves nothing half-done.
- [ ] Images, if any, are declared with `with_result_images`.
- [ ] It is registered through `with_tools` or `host_tools::register`, before anything is served.
- [ ] There are tests for the working call, a bad argument, the gate, and the name.
