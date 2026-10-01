# Design note: the local link's transports

**Status: implemented; native Windows verification pending.** The transport seam,
SID-named pipes, detached service lifecycle, and Job Object shell are in the tree.
The Unix tests pass with their test bodies unchanged. The Windows link, capability
wrapper, and shell tests cross-compile and lint on macOS; runtime evidence must come
from `.github/workflows/local-transports.yml` on `windows-latest` before Windows support
is called verified. This note implements the link and process-lifecycle portion of
roadmap item 29.

## The decision

The link keeps **one protocol and two local transports, chosen at compile time**:

| Platform | Transport | Endpoint |
|---|---|---|
| Unix (Linux, macOS) | Unix domain socket | a path under `<nanus home>/run/` |
| Windows | Named pipe | `\\.\pipe\nanus-<sid>-<name>` |

A Unix build compiles only the socket and a Windows build compiles only the pipe, with
`#[cfg(unix)]` and `#[cfg(windows)]`. Nothing is chosen at run time, neither build carries
the other's code or dependencies, and a Windows client and a Unix agent never meet — this
is a *local* link, and a pipe and a socket cannot talk to each other anyway. Reaching an
agent from another machine is a separate mechanism, described in
[the remote link note](remote-link.md), and it is built on top of this one rather than
replacing it.

The protocol does not change. The frames, the handshake, the one-JSON-object-per-line
framing, `PROTOCOL_VERSION`, and the 4 MiB `MAX_FRAME_BYTES` cap are identical on both.
A new transport is therefore not a reason to move `PROTOCOL_VERSION`: no frame's meaning
changes.

## Why not one transport everywhere

Loopback TCP would be a single code path on every platform, and it is the wrong trade here.
The link's trust model is *the filesystem*: a socket that is `0600` inside a `0700`
directory can only be reached by the user it belongs to, and the docs lean on that to say
the link "trusts its peer" without defending against anything a same-user process could not
already do. A TCP port can be reached by any local user and any local process, so adopting
it would need a token or a certificate to put back what the filesystem gave for free — and
it would do that on every platform to serve one. A named pipe is the Windows analogue of the
socket: a local, kernel-mediated endpoint with an access-control list, which is the property
the model depends on.

## Where Unix is baked in today

The Unix-specific surface is small, which is what makes this tractable:

- `nanus-link/src/client.rs` — `UnixStream` and its owned halves, in `Client` and
  `Client::open`.
- `nanus-link/src/server.rs` — `bind` (the stale-socket probe, the `0700` directory, the
  `0600` socket), the `UnixListener` accept loop, and the connection handler that takes a
  `UnixStream`.
- `nanus-link/src/paths.rs` — returns `PathBuf`s for `run/agent.sock` and
  `run/attach-<pid>.sock`.
- `nanus-cli/src/service.rs` — `setsid` to detach, and `SignalKind` for shutdown (also in
  `cli.rs`).
- The shell adapter's `nix` process-group kill, which is not the link and is handled in its
  own change (below).

`nanus-tui` has no direct dependency on the socket type, so it follows once the client is
generic over the stream.

## The seam

Introduce a transport module in `nanus-link` and make everything above it generic.

- A `Stream` that is `AsyncRead + AsyncWrite + Unpin`, split into an owned read half and an
  owned write half, because `Client::split` hands the halves to different tasks.
- A `Listener` that accepts `Stream`s.
- An `Endpoint` that is the value `paths.rs` returns: a filesystem path on Unix, a pipe name
  on Windows. A client computes it and a server binds it; the two never exchange it, which
  is the property `paths.rs` already documents and which has to survive.
- `Client::open` takes a stream rather than a `UnixStream`. **Decided: a concrete enum over
  the two transports, not a boxed trait object.** Each platform compiles one variant, so the
  enum is a single type on every build; there is no allocation and no dynamic dispatch, and
  the platform split is visible in the type rather than hidden behind it. The generic
  parameter does not spread into `nanus-tui`, because the interface names the enum rather
  than a type parameter.

The existing Unix tests, which use `UnixStream::pair`, must pass without being edited. If
they need editing, the seam is in the wrong place.

## Windows specifics

**Endpoint naming.** A pipe name is global to the machine, not scoped to a directory, so
the user has to be in the name or two users' agents collide. **Decided: the user is
identified by SID, not account name.** A SID is stable and cannot collide with a display
name or be spoofed by one, which an account name can. The cost is that it is long and not
something a person types, so the name is always computed (as `paths.rs` already computes
socket paths) and never entered by hand. The service takes one name per user and a
shell-scoped agent one per process, mirroring `agent.sock` and `attach-<pid>.sock`:
`\\.\pipe\nanus-<sid>-agent` and `\\.\pipe\nanus-<sid>-attach-<pid>`.

**Access control.** The intent is to match `0600`: only the creating user may connect.
**Decided: the default pipe security descriptor is sufficient, so no explicit descriptor is
set.** Create the pipe with `first_pipe_instance(true)` so a squatter that created the name
first is an error rather than a man in the middle, and with `reject_remote_clients(true)` so
the pipe is never reachable over SMB.

The decision is a claim about what the default grants, and the default grants *something*
to accounts other than the owner (read access to Everyone, as the platform documents it), so
it is held to a test rather than trusted: a test reads the descriptor of a created pipe back
and asserts which principals can write to it. A cross-user connection test is deferred (see
[Tests](#tests)). If the read-back fails, this decision is wrong and the `unsafe`
question below reopens.

**No stale endpoints.** A pipe disappears when its last handle closes, so the Unix
`bind` dance — probe the existing socket, remove it if nothing answers — has no Windows
counterpart, and the Windows `bind` is simpler for it. A second service on one machine is
still a collision, reported by `first_pipe_instance`.

**Accepting.** A pipe server must create the next instance *before* handing the connected
one off, or there is a window in which a client finds no pipe. The accept loop owns that.

## The `unsafe` constraint

The repository forbids `unsafe` at the workspace level (`unsafe_code = "forbid"`) and says
there must never be any. The rule stays. Windows has two jobs this design needs that only
the Win32 API can do — reading the current user's SID (for the pipe name) and a Job Object
with kill-on-close (for the shell) — and calling Win32 is FFI, which Rust treats as
`unsafe`. The pipe's explicit security descriptor would have been a third, and it is gone
because the default descriptor is accepted.

**Decided: one exception, in one crate.** A single wrapper crate, working name
`nanus-sys-windows`, is the only place in the repository allowed to contain `unsafe`. It
exists to turn those few Win32 calls into a small safe API, in the same arrangement as
`nix` for process groups, except that the vetted crate is ours. Everything else — the
link, the shell adapter, the service, the interface — calls that safe API and stays
`unsafe`-free, and keeps `forbid`.

The terms of the exception, so that it cannot grow quietly:

- **One crate, no others.** A second crate with `unsafe` is a new decision, not a
  precedent. The wrapper is `cfg(windows)`: on a Unix build it is empty and pulls in
  nothing.
- **A narrow surface.** It exposes the capabilities, not the calls: a current-user SID as
  a string, and a job handle that kills its processes on drop. It does not export a raw
  handle, a pointer, or a type from the FFI crate it uses, so callers cannot reintroduce
  `unsafe` by holding one.
- **Wrap before writing.** Prefer a maintained crate that already provides each
  capability safely, and write FFI only for what none does. Every `unsafe` block carries a
  `// SAFETY:` comment that states the invariant it relies on, and the crate is small
  enough to read in one sitting.
- **Assertions at the boundary.** The wrapper checks what the OS returns (a non-empty SID,
  a handle that is valid) and returns an error from the crate's own error enum rather than
  trusting it. The Windows tests exercise it on `windows-latest`.
- **The rest of the tree is held to the rule by a check, not by memory.** The other crates
  keep `#![forbid(unsafe_code)]`, and CI fails if `unsafe` appears anywhere outside the
  wrapper.

### The lint override

A workspace-level `forbid` cannot be relaxed by a member crate that inherits it. This was
checked rather than assumed: in a scratch workspace with `unsafe_code = "forbid"`, a member
with `[lints] workspace = true` fails on an `unsafe` block, and a member that declares its
own `[lints]` table compiles it.

**Decided: the wrapper opts out of `[lints] workspace = true` and declares its own table.**
The workspace keeps `forbid`, so every other crate is unchanged and still cannot write
`unsafe`. The wrapper's manifest restates the workspace's other lints verbatim and sets only
`unsafe_code = "allow"` (or `"deny"` with `#[allow(unsafe_code)]` on the few modules that
need it, which is narrower and preferred). Restating the table is the cost: the two copies
can drift, so the wrapper carries a test that parses both manifests and fails if the
wrapper's table differs from the workspace's in anything but `unsafe_code`.

The override is **not added yet**. A lint table for a crate that does not exist is dead
configuration, and it is the kind of exception that outlives its reason. It lands in the
same commit as the wrapper crate, together with the statement of the exception in
`AGENTS.md` and `docs/design.md`, both of which say today that there is no `unsafe` and
there must never be.

### What a survey of the crates found

The two capabilities were checked against crates.io and the repositories (versions and dates
as of this note; re-check them before depending on one).

| Capability | Crate | Finding |
|---|---|---|
| Job Object, kill-on-close | `win32job` 2.0.3 | A safe API for exactly this: `Job::create_with_limit_info`, `ExtendedLimitInfo::limit_kill_on_job_close`, `assign_process`, and a `Drop` that closes the handle. Depends on `windows` 0.61 and `thiserror` 1. Last pushed June 2025, one maintainer, not archived. It also exposes the raw handle as an `isize`, which the wrapper must not re-export. |
| Job Object, spawning | `process-wrap` 10.0.1 | Wraps a `Command` to spawn into a process group, session, or Job Object, with `std` and `tokio` front ends and `nix` on Unix. Updated September 2026, from the `watchexec` project, far more used than `win32job`. It owns spawning, so it would replace how the shell adapter spawns rather than add to it. `command-group` is its predecessor and is stale (November 2023). |
| Current user's SID | `winsafe` 0.0.29 — **chosen** | Safe, idiomatic Win32 bindings, updated September 2026. Under its `advapi` feature it has `HACCESSTOKEN::GetCurrentProcessToken`, `GetTokenInformation`, and a `SID` type that implements `Display`. Pre-1.0 (`0.0.x`), so the API may move. It is a large surface, of which one call is wanted. **Verified to type-check for `x86_64-pc-windows-msvc` under `#![forbid(unsafe_code)]`** with features `advapi` and `kernel`: `HPROCESS::GetCurrentProcess().OpenProcessToken(co::TOKEN::QUERY)`, then `GetTokenInformation(co::TOKEN_INFORMATION_CLASS::User)` returns `TokenInfo::User(Box<TOKEN_USER>)`, and `.User.Sid()` returns an `Option` of a `SID` whose `to_string()` is the SDDL form. It type-checked first and has since been **run**: `examples/windows-sid-probe` passes on `windows-latest` in `.github/workflows/embedding.yml` (run 36836772373, commit `ba7eb38`), where the SID is a well-formed `S-1-…` string, is stable between calls, and is not LocalSystem's `S-1-5-18`. |
| Current user's SID | `windows-permissions` 0.2.4 | A safe wrapper over exactly this area, but not updated since June 2021 and built on `winapi`, which is unmaintained. Not recommended. |
| Current user's SID | `windows-acl`, `whoami` | `windows-acl` is stale (January 2021) and about ACLs. `whoami` returns an account name, not a SID. Neither fits. |
| Raw bindings | `windows` 0.62, `windows-sys` 0.61 | Microsoft's own, current, and entirely `unsafe` to call. This is what a hand-written wrapper would sit on. |

What this changes is the size of the exception. Both capabilities exist in maintained crates
with safe APIs, which is the same arrangement the repository already uses for `nix`: the
`unsafe` lives in a dependency and the workspace's `forbid` stays absolute. So the
preferred shape is a `nanus-sys-windows` crate that **contains no `unsafe` of its own** and
only narrows those crates to the two calls the design needs. In that shape the wrapper
needs no override, and the exception is unused.

The override above is therefore the **fallback**: it is what lets the wrapper write the FFI
itself if a crate proves unfit — `winsafe` pre-1.0 churn, or `win32job`'s single maintainer
— without reopening the decision. Choosing between depending on a crate and writing the FFI
is made when the wrapper is built, against the versions then current, and recorded here.

## Process lifecycle, which is not the link

The service cannot run on Windows until these are ported, but they are separate changes with
separate risk, and they should not ride on the transport change:

- **Detaching.** `setsid` becomes `CreateProcess` with `DETACHED_PROCESS` and
  `CREATE_NEW_PROCESS_GROUP`, available through `std::os::windows::process::CommandExt`
  without `unsafe`.
- **Supervision.** **Decided: no Service Control Manager integration.** The service is a
  detached process, or `--foreground` under a supervisor, exactly as on Unix. The reasons
  are that a service needs a second lifecycle path alongside the one the design says there
  should be only one of; that it would run as a different SID from the person who wants to
  reach it, so with SID-named pipes their interface would not find it; that `LocalSystem`
  is unacceptable for an agent that runs a shell; and that running as the user means
  storing their password with the SCM. `service.md` should document, when this ships, the Windows
  supervisors that fit instead — Task Scheduler with an *at log on* trigger runs as the user with no stored
  password and no administrator rights, and NSSM-style wrappers work too. The cost is
  stated rather than hidden: a detached process outlives the shell, but is ended when the
  user logs off, and nothing starts it at boot with nobody logged in. If that is ever
  wanted it is its own feature — `nanus service install --user <account>`, never
  `LocalSystem` — with the password handling designed on purpose.
- **Stopping.** There is no `SIGTERM`. `service stop` already asks over the link and sends
  no signal, so it carries over unchanged; Ctrl-C under `--foreground` becomes
  `tokio::signal::windows::ctrl_c`.
- **The shell tool.** `sh -c` and the process-group kill are Unix, and the docs explain why
  the group matters (a grandchild does the work, and `kill_on_drop` cannot reap it). The
  Windows equivalent is a Job Object with kill-on-close. It is the largest piece and the one
  most likely to need the `unsafe` decision above.

## Build order

1. **Extract the seam.** Generic `Client` and `serve_connection`, the Unix transport behind
   it, no behaviour change. All existing tests pass unedited.
2. **Add the Windows transport**, and run `nanus-link`'s tests on `windows-latest`. The
   `embedding.yml` matrix is the model.
3. **Port the service lifecycle** (detach and foreground shutdown).
4. **Port the shell** with a Job Object, as its own change.
5. **Move the docs**: `status.md`, `features.md`, `service.md`, and roadmap item 29, in the
   same change that makes each sentence true and not before.

## Tests

- The existing socket tests stay as they are and run on Unix only.
- The Windows transport gets the same cases, not weaker ones: a handshake, a split
  connection reading and writing independently, a refused attachment keeping the agent's
  message, a peer that hangs up before its handshake. Both directions, as everywhere here.
- A test that a second owner of one pipe name is refused.
- A test that a pipe created by one user cannot be opened by another is **deferred**: it needs
  two accounts, and nanus is assumed to run in a single-user, sandboxed environment, so the
  cross-user case is not a threat this version defends against. The read-back test of the
  default descriptor stays, because it needs one account. The deferral is a stated limit, not
  a silent skip: `SAFETY.md` and `service.md` say, when this ships, that the Windows link does
  not defend against another user on the same machine.
- The platform tests are gated by `cfg`, so each OS runs its own and neither runs the
  other's.

## Implementation decisions and verification

- `nanus-sys-windows` pins `winsafe` to `=0.0.29` for the SID and uses `process-wrap`
  10.0.1 for spawning into a kill-on-close Job Object. The latter suspends the process
  until assignment and resumes it afterwards: assigning an already running child with
  `win32job` would leave a window for a grandchild to escape. The wrapper exposes no raw
  handles or FFI types. Its liveness probe checks job cleanup without making an inaccessible
  process look dead.
- No Rust `unsafe` or lint override was needed. The fallback exception above remains unused;
  every crate, including the wrapper, inherits the workspace's forbid and states it in its
  crate root. CI checks those properties, so there is no duplicate lint table to drift.
- Unix retains Tokio's owned socket halves. Windows splits the concrete stream with Tokio's
  safe owned halves; the enum itself uses no boxing or dynamic dispatch.
- `Client::open` accepts `Into<Stream>`, and `serve` accepts `Into<Listener>`, to keep existing
  Unix fixtures source compatible. Both protocol implementations still use `wire.rs`, the same
  protocol version, and the same frame cap.
- The descriptor read-back uses Windows PowerShell's .NET Framework
  `PipeStream.GetAccessControl`, on a pipe created by the actual transport. It checks that
  writers are the current SID, LocalSystem, and Administrators, and that Everyone and Anonymous
  have only read access. It introduces no Rust FFI. This is intentionally a check of the default
  descriptor's actual grants, not a claim that it equals Unix `0600`.
- Windows shell tests exercise output bounds, stdin, failed spawning, timeout, dropped-run
  cancellation, streamed shutdown, and death of a real grandchild. Service shutdown continues
  to use the same link request; Windows foreground processes also watch Ctrl-C.
- The cross-user connection test remains deferred. The native Windows ACL, pipe, Job Object,
  detached-service smoke test, and binary build results are still pending; cross-compilation is not runtime evidence.

## Decided

| Question | Decision |
|---|---|
| How the user is named in a pipe | **SID**, always computed, never typed |
| Is the default pipe descriptor enough | **Yes**, held to a read-back test |
| Boxed stream or concrete enum | **Concrete enum** over the two transports |
| Service Control Manager integration | **None**; detached process or `--foreground` under a supervisor |
| The `unsafe` rule | **Kept**, with a single exception: one Windows wrapper crate, a narrow safe API, nothing else |
| How the wrapper may contain `unsafe` | By opting out of `[lints] workspace = true` with its own table, differing from the workspace's only in `unsafe_code`; **not added until the crate exists**, and unused if safe crates suffice |
| Cross-user pipe test | **Deferred**; single-user sandboxed environments assumed, stated as a limit |
| The SID lookup | **`winsafe`**, pinned to an exact version (`=0.0.29`) because it is pre-1.0, with only the `advapi` and `kernel` features, depended on rather than writing FFI; held to `examples/windows-sid-probe` in CI |
