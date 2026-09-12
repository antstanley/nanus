# Sandboxed / streaming subprocess runner for a headless CLI agent harness

Target: Rust 1.98.0 stable, macOS aarch64, tokio 1.53.1. Need: `sh -c "<script>"`, incremental
stdout/stderr streaming, wall-clock timeout, per-stream output cap, and **reliable kill of the
whole process tree** (shell + grandchildren).

**Recommendation up front: option 1 — `tokio::process::Command` directly, with
`.process_group(0)` + explicit `libc::killpg(pgid, SIGKILL)` on timeout, and read-to-EOF with a
byte cap.** No extra crate is needed, no extra crate removes any of the work, and
`process-wrap`'s `KillOnDrop` does **not** group-kill on Unix (measured below). `process-wrap` is
the right choice only if you want its group-*reaping* semantics (no zombie grandchildren) and are
willing to ship an extra dependency for ~15 lines of equivalent code.

---

## 1. `tokio::process` directly

**What tokio 1.53.1 actually offers:**

| Question | Answer | How verified |
|---|---|---|
| `Command::process_group(i32)` on `tokio::process::Command`? | **Yes — inherent method, Unix-only.** No `std::os::unix::process::CommandExt` import required. | compiled |
| `Command::kill_on_drop(bool)`? | **Yes — inherent.** | compiled |
| Any built-in group kill? | **No.** `Child`'s entire method set is `id`, `kill`, `start_kill`, `try_wait`, `wait`, `wait_with_output`, `raw_handle`. `kill()` = SIGKILL to that one PID. | docs.rs + runtime |
| Does `kill_on_drop(true)` kill only the direct child? | **Yes. Grandchildren survive.** | runtime, case A below |

`process_group(0)` sets `PGID = child PID`; the child is therefore the leader of a fresh group and
`pgid == child.id()`, so you can `killpg(pgid)` without an extra `getpgid` call.

**Measured behaviour** (`sh -c 'sleep 300 & echo $! > f; wait'`, macOS aarch64, tokio 1.53.1):

| Case | Action | leader alive | **grandchild alive** |
|---|---|---|---|
| A | `.kill_on_drop(true)` + `.process_group(0)`, then **drop** the `Child` | no | **YES** |
| B | `.process_group(0)`, then `libc::killpg(pid, SIGKILL)` | zombie until `wait` | no |
| C | no `process_group`, then `Child::kill().await` | no | **YES** |
| D | process-wrap `ProcessGroup::leader()` + `.kill().await` | no | no |
| E | process-wrap `ProcessGroup::leader()` + `KillOnDrop`, then **drop** | no | **YES** |

Case A is the trap: `kill_on_drop` is a *backstop for the direct child only*. Case C is what you
get if you forget `process_group(0)` entirely — and it is exactly the orphaned-grandchild bug an
agent harness must not have (`sh -c "cargo build"` finishing while `rustc` children keep running).

Two further verified details:

* **`Child::wait()` is cancel-safe.** `tokio::time::timeout(dur, child.wait())` can be cancelled,
  and a second `child.wait().await` after `killpg` correctly reaps and returns the status
  (signal-killed → `status.code() == None`).
* **`AsyncReadExt::take(n)` is the wrong truncation primitive on a pipe.** It *stops reading* after
  `n` bytes. The child then blocks forever on a full pipe (64 KiB on macOS) — a hang, not a
  truncation. Measured: `take(4096)` returned 4096 bytes, the writer was still running and had to
  be killed. Truncate by **reading to EOF and discarding** past the cap.

**Recommended pattern** (this is the sketch in §6): `process_group(0)` + `kill_on_drop(true)`,
two `tokio::spawn`ed line pumps feeding a **bounded** channel, `tokio::time::timeout(child.wait())`,
on elapse `killpg(pgid, SIGKILL)` then re-`wait()`, then join the pumps (which hit EOF once the
group dies).

---

## 2. `process-wrap` 10.0.0

Maintained successor to `command-group`, same author (Félix Saparelli / watchexec). Published
2026-08-24, **MSRV 1.87.0**, edition 2024, resolver 3. Not a monolithic cross-platform API —
composable single-concern wrappers.

**Wrappers present in 10.0.0** (README + `docs.rs/process-wrap/10.0.0/process_wrap/tokio/`):
`ProcessGroup` / `ProcessGroupChild`, `ProcessSession`, `KillOnDrop`, `ResetSigmask`,
`CreationFlags` (Windows), `JobObject` (Windows). **There is no `Timeout` wrapper and no `Resize`
wrapper** — those do not exist in 10.0.0 despite being natural candidates. Timeout and truncation
remain yours to implement either way.

`ProcessGroupChild` is the genuinely useful part: `start_kill()` calls
`nix::sys::signal::killpg(pgid, SIGKILL)` and `wait()` reaps the whole group with
`libc::waitpid(-pgid, …, WNOHANG)` in a loop, so grandchildren that reparent are reaped rather than
left as zombies. Verified by reading `src/tokio/process_group.rs`.

**`KillOnDrop` is *not* a group kill.** It is a shim whose entire body is
`command.kill_on_drop(true)` (read from `src/tokio/kill_on_drop.rs`), so on Unix it inherits the
tokio semantics above: **direct child only**. Confirmed at runtime (case E): grandchild survived
a drop of a `ProcessGroup` + `KillOnDrop` child. There is no `Drop` impl on the wrap child that
would route through `start_kill`.

**Cargo.toml** (exact, minimal — avoids the Windows-only default features; both lines compile):

```toml
# Recommended if you use it: feature name is exactly `tokio1`, NOT `tokio`.
process-wrap = { version = "10.0.0", default-features = false, features = ["tokio1", "process-group", "kill-on-drop"] }

# Or, simplest, pull the default set (Windows wrappers are target-gated and not built on macOS):
process-wrap = { version = "10.0.0", features = ["tokio1"] }
```

Declared features (from upstream `Cargo.toml`): `std`, `tokio1 = ["dep:nix", "dep:futures", "dep:tokio"]`,
`creation-flags`, `job-object`, `kill-on-drop`, `process-group`, `process-session`, `reset-sigmask`,
`tracing`. Defaults = `creation-flags, job-object, kill-on-drop, process-group, process-session, tracing`.
`tokio1` does **not** imply `std`, and you must keep `process-group` enabled yourself when using
`default-features = false`.

**Usage sketch** (compiled):

```rust
use process_wrap::tokio::*;

let mut child = CommandWrap::with_new("sh", |cmd| {
        cmd.arg("-c").arg(script).stdout(Stdio::piped()).stderr(Stdio::piped());
    })
    .wrap(ProcessGroup::leader())   // sets Command::process_group(0)
    .wrap(KillOnDrop)               // backstop only — NOT a group kill on Unix
    .spawn()?;
// ... on timeout:
std::pin::Pin::from(child.kill()).await?;  // killpg(SIGKILL) + group reap
```

Ergonomics caveat: `ChildWrapper::kill()` returns `Box<dyn Future + Send>` which is **not `Unpin`**,
so `child.kill().await` does not compile — you must write
`std::pin::Pin::from(child.kill()).await`. That is a real papercut against the "simplest safe"
criterion. Also `ProcessGroupChild::stdout()`/`stderr()` and `stop`/stream accessors are awkward to
use through the `Box<dyn ChildWrapper>` chain.

---

## 3. `duct` 1.1.2

MIT, oconnor663, published 2026-09-03, no declared MSRV, deps `os_pipe 1.2.3`, `shared_child 1.1.2`,
`shared_thread`. **Synchronous only** (`std::process` + internal threads) — every call blocks the
calling thread, so inside a tokio runtime it must be wrapped in `spawn_blocking`. Cross-platform
(Unix + Windows).

Semantics of the three read modes:

| API | Meaning |
|---|---|
| `.run()` | inherit stdio, write straight to the terminal; capture nothing. |
| `.read()` | run to completion, capture **stdout at the end** as `String`; errors on non-zero status unless `.unchecked()`. Not streaming. |
| `.stdout_capture()` / `.stderr_capture()` | builder modifiers that only *configure* capture (piped); the `Expression` still needs `run`/`read`/`start`. Capture-at-end, buffered in memory. |
| `.reader()` | **the only true incremental read path**: starts the child and returns a `ReaderHandle` implementing `Read`. `stderr_to_stdout().reader()` merges both streams. This is genuine streaming. |

**Timeout:** there is no `Expression::timeout()` builder. The `timeout` feature (on by default,
`timeout = ["shared_child/timeout"]`) only adds `Handle::wait_timeout(Duration)` /
`wait_deadline(Instant)` → `Result<Option<&Output>>`. It **does not kill anything**; it returns
`None` when the deadline passes and you must kill yourself. `ReaderHandle` has **no**
`wait_timeout` at all, so streaming + timeout is a manual thread + kill dance.

**Group kill: no.** Upstream `Cargo.toml`/sources contain no `setpgid`/`process_group` call — duct
never creates a new process group. `Handle::kill()` is documented verbatim: *"this does not kill any
grandchild processes that the children have spawned on their own. It only kills the child processes
that Duct spawned itself."* It also documents the follow-on trap for `ReaderHandle`: an unkilled
grandchild keeps the stdout pipe open and keeps your reader thread blocked. `Handle::pids()` and the
`unix::HandleExt::send_signal()` exist, but signalling the known child PIDs still does not reach
grandchildren. Handled through `shared_child`, whose `kill()` is just
`std::process::Child::kill()` = SIGKILL to one PID.

Verdict: duct is a good shell-pipeline ergonomics library, but it fails the "reliably kill the whole
tree" requirement and adds a sync/async bridge. Not the right tool here.

---

## 4. `command-group` 5.0.1

**Dead.** Not archived on GitHub, but:
* crates.io `max_stable_version` = 5.0.1, published **2023-11-18**, MSRV 1.68.0 — no release in ~3 years.
* GitHub repo description is literally **"Deprecated: use process-wrap."**
* Last commit on `main`: **2024-04-21** ("More prominently announce succession"); previous commit
  "Soft-launch process-wrap".
* README opens with: *"The successor of command-group is process-wrap. **No further work will be
  done on command-group.**"*
* 5.0.1 is not yanked, so it still resolves, but it is a compatibility artifact only.

**A new 2026 project should use `process-wrap`, not `command-group`.** (Or, per §6, neither.)

---

## 5. Output truncation

**There is no mature crate for this.** crates.io searches for "output limit", "stream capture
subprocess", "bounded buffer" surface only unrelated crates (`pid`, `ring-channel`,
`dasp_ring_buffer`, agent-harness tools). This is a ~5-line concern, not a dependency.

* **Hand-rolled cap over `String`/`Vec<u8>` is the right answer.** Append while
  `len + line.len() + 1 <= cap`, then set `truncated = true` and keep *draining* (never stop
  reading — see the `take(n)` trap in §1).
* **`bytes::BytesMut`** (bytes 1.12.1) is relevant only if you want zero-copy chunk handoff to the
  UI (`split_to`/`freeze` → `Bytes`), and it has **no built-in cap or truncation** — you would still
  write the same length check. Not worth a dependency on its own; `bytes` is already in the tokio
  tree transitively via `tokio-util`, but not via tokio alone.
* Cap **per stream** (stdout and stderr separately); a combined cap lets a noisy stderr starve
  stdout.
* `AsyncReadExt::take(n)` is **not** the streaming+truncate tool on a pipe (measured: it stalls the
  writer on a full pipe). The correct shape is: read to EOF, keep ≤ cap, mark truncated.
* **Second memory hazard, measured:** forwarding every line to an **unbounded** channel is itself
  unbounded. In a test run, `yes | head -c 50MB` enqueued **4,545,459** events into an
  `mpsc::unbounded_channel` because the consumer could not keep up. Use `mpsc::channel(N)` with
  `try_send` and drop-or-coalesce on `Full`. Truncating the stored buffer does not protect you if
  the live stream is unbounded.

---

## 6. Final recommendation

**Use `tokio::process::Command` directly, with `libc` (or `nix`) for `killpg`.**

Why: it is the only option that is simultaneously (a) zero extra dependencies beyond what the
workspace already has, (b) genuinely streaming, (c) able to kill the whole tree, (d) bounded in
memory. `process-wrap` is the closest contender and is genuinely maintained, but `KillOnDrop` — the
wrapper you would reach for to get "no orphans" — does not group-kill on Unix, it has no `Timeout`
wrapper, and its `kill()` future needs `Pin::from(...)` to even `.await`. `duct` cannot group-kill
at all and is sync-only. `command-group` is deprecated.

**The subprocess-shell case (`sh -c "..."`) explicitly:** spawning `sh -c` means the direct child is
a *shell*, and every real tool (`cargo`, `make`, `npm`, a pipeline) is a grandchild. So
`Child::kill()` and `kill_on_drop(true)` are both insufficient by construction — measured, cases A
and C killed the shell while the grandchild kept running. The only reliable fix is
`process_group(0)` at spawn (shell becomes group leader, `pgid == pid`) plus `SIGKILL` **to the
negative pgid** on timeout/cancel. That is exactly what case B/D do, and it is why the sketch below
uses `libc::killpg`.

**Cargo.toml lines:**

```toml
[workspace.dependencies]
# already present; `time` is required for the timeout, `sync` for the progress channel.
tokio = { version = "1.53", features = ["rt-multi-thread", "macros", "process", "io-util", "time", "sync"] }

[target.'cfg(unix)'.dependencies]
libc = "0.2"          # for killpg only; nix = { version = "0.31", features = ["signal"] } also works
```

The workspace currently declares tokio without `rt-multi-thread`; the CLI will need it (or you must
stay on the current-thread runtime).

**Verified runner (~55 lines, compiles and runs on tokio 1.53.1 / macOS aarch64):**

```rust
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::{self, Sender};
use tokio::task::JoinHandle;

pub struct Outcome {
    pub code: Option<i32>,      // None == killed by a signal
    pub timed_out: bool,
    pub truncated: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Drain one pipe line-by-line: stream every line live, keep at most `cap` bytes.
fn pump<R: AsyncRead + Unpin + Send + 'static>(
    r: R, is_err: bool, cap: usize, sink: Sender<(bool, String)>,
) -> JoinHandle<(String, bool)> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(r).lines();
        let (mut buf, mut truncated) = (String::new(), false);
        while let Ok(Some(line)) = lines.next_line().await {
            // Bounded channel + try_send: never let a slow UI grow memory without limit.
            let _ = sink.try_send((is_err, line.clone()));
            if buf.len() + line.len() + 1 <= cap {
                buf.push_str(&line);
                buf.push('\n');
            } else {
                truncated = true;   // keep reading to EOF; just stop storing
            }
        }
        (buf, truncated)
    })
}

/// Run `sh -c script`. Streams live, caps memory, times out, and kills the WHOLE process group.
pub async fn run_shell(
    script: &str, timeout: Duration, cap_per_stream: usize, sink: Sender<(bool, String)>,
) -> std::io::Result<Outcome> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)      // Unix-only, inherent on tokio 1.53: child becomes group leader
        .kill_on_drop(true);   // backstop for early-drop only; kills the DIRECT child, not the group
    let mut child = cmd.spawn()?;
    let pgid = child.id().expect("spawned child has a pid") as i32; // == PGID thanks to process_group(0)

    let h_out = pump(child.stdout.take().unwrap(), false, cap_per_stream, sink.clone());
    let h_err = pump(child.stderr.take().unwrap(), true, cap_per_stream, sink);

    let (timed_out, status) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(st) => (false, st?),                       // Child::wait is cancel-safe
        Err(_) => {
            unsafe { libc::killpg(pgid, libc::SIGKILL); }   // shell AND every grandchild
            (true, child.wait().await?)                     // reap; signal death -> code() == None
        }
    };
    let (stdout, t1) = h_out.await.expect("pump task panicked"); // pipes EOF once the group dies
    let (stderr, t2) = h_err.await.expect("pump task panicked");
    Ok(Outcome { code: status.code(), timed_out, truncated: t1 || t2, stdout, stderr })
}
```

End-to-end run of the same logic produced:

```
1) code=Some(3) timeout=false trunc=false stdout="out1\nout2\n" stderr="err1\n"
2) timeout=true code=None partial_stdout="before-timeout\n" grandchild_alive=false
3) timeout=false stdout_len=99 truncated=true          # cap=100, 50 MB producer, no hang
4) streamed 4545459 events live, first=Some((false, "out1"))   # <- the unbounded-sink hazard
```

Notes for integration: use `libc::waitpid`/`read` nothing yourself — `child.wait()` after `killpg`
reaps the leader; grandchildren that reparent to `launchd` are reaped by the OS. If you also need
the *zombie-free* group-reap guarantee that `process-wrap::ProcessGroupChild::wait()` provides
(loop of `waitpid(-pgid, WNOHANG)`), that loop is ~10 lines and is the only feature of process-wrap
this recommendation gives up.

---

## Verified by compiling / running vs. read only

**Verified by compiling (rustc 1.98.0, tokio 1.53.1, macOS aarch64):**
* `tokio::process::Command::process_group(0)` exists and is callable as an inherent method with no
  `std::os::unix::process::CommandExt` import.
* `tokio::process::Command::kill_on_drop(true)` exists as an inherent method.
* `.process_group(0)` + `.kill_on_drop(true)` + `BufReader::new(stdout).lines()` +
  `AsyncReadExt::take(n)` + `tokio::time::timeout(_, child.wait())` all co-compile in one function.
* `process-wrap = { version = "10.0.0", default-features = false, features = ["tokio1", "process-group", "kill-on-drop"] }`
  compiles and `CommandWrap::with_new(...).wrap(ProcessGroup::leader()).spawn()` works.
* `process-wrap::tokio::ChildWrapper::kill()` returns a non-`Unpin` `Box<dyn Future>`; `.await`
  requires `std::pin::Pin::from(...)`.

**Verified by running:**
* Case A: tokio `kill_on_drop(true)` on a group leader — **grandchild survives**.
* Case B: `libc::killpg(pgid, SIGKILL)` — grandchild dies; leader is a zombie until `wait`.
* Case C: plain `Child::kill().await` with no `process_group` — **grandchild survives**.
* Case D: process-wrap `ProcessGroup::leader()` + `.kill().await` — leader and grandchild both die,
  no zombie.
* Case E: process-wrap `ProcessGroup::leader()` + `KillOnDrop` + **drop** — **grandchild survives**.
* `Child::wait()` is cancel-safe: after `timeout()` elapsed and `killpg` ran, a second
  `child.wait().await` returned the (signal) status.
* `.take(4096)` returns 4096 bytes but leaves the producer running/blocked — not a truncation primitive.
* Full runner: correct exit code and stream separation; timeout kills the grandchild and still
  returns partial output; 100-byte cap on a 50 MB producer terminates cleanly (no pipe deadlock).
* Unbounded mpsc sink accumulated 4,545,459 events from a fast producer.

**Read only (not executed):**
* `docs.rs/duct/1.1.2` API surface (`Expression`/`Handle`/`ReaderHandle` method lists,
  `Handle::wait_timeout` returning `Option<&Output>`), duct `src/lib.rs`, `src/unix.rs`,
  `Cargo.toml` feature table, and duct's documented "does not kill grandchildren" contract.
* `shared_child` `wait_timeout`/`wait_deadline`/`kill` source (timeout does not kill; `kill` is
  `std::process::Child::kill`).
* `process-wrap` `src/tokio/process_group.rs` (`killpg` + `waitpid(-pgid, WNOHANG)` reap loop),
  `src/tokio/kill_on_drop.rs` (body = `command.kill_on_drop(true)`), `src/tokio/core.rs`
  (`ChildWrapper` default `kill`/`start_kill`), `README.md`, `Cargo.toml`.
* `docs.rs/tokio/1.53.1` `Child`/`Command` method lists and `kill_on_drop`/`process_group` prose.
* crates.io metadata for process-wrap, command-group, duct, bytes.
* GitHub API: `command-group` archived=false, pushed_at 2024-04-21, description
  "Deprecated: use process-wrap.", commit log, 5.0.1 not yanked.
* crates.io keyword searches: no mature output-truncation crate.

**Not verified:** Windows behaviour of any of the above; `process-wrap`'s Windows `JobObject` /
`CreationFlags` paths; whether `process-wrap` 10.0.0 keeps `KillOnDrop`-as-group-kill as an
intentional design (the README lists it as a shim, which matches the measured Unix behaviour).

## URLs fetched

* https://crates.io/api/v1/crates/process-wrap, .../command-group, .../duct, .../bytes
* https://raw.githubusercontent.com/watchexec/process-wrap/main/README.md
* https://raw.githubusercontent.com/watchexec/process-wrap/main/Cargo.toml
* https://raw.githubusercontent.com/watchexec/process-wrap/main/src/tokio/process_group.rs
* https://raw.githubusercontent.com/watchexec/process-wrap/main/src/tokio/kill_on_drop.rs
* https://raw.githubusercontent.com/watchexec/process-wrap/main/src/tokio/core.rs
* https://raw.githubusercontent.com/watchexec/command-group/main/README.md
* https://api.github.com/repos/watchexec/command-group, .../commits?per_page=3
* https://raw.githubusercontent.com/oconnor663/duct.rs/master/Cargo.toml
* https://raw.githubusercontent.com/oconnor663/duct.rs/master/src/lib.rs
* https://raw.githubusercontent.com/oconnor663/duct.rs/master/src/unix.rs
* https://raw.githubusercontent.com/oconnor663/shared_child.rs/master/src/lib.rs
* https://docs.rs/process-wrap/10.0.0/process_wrap/
* https://docs.rs/process-wrap/10.0.0/process_wrap/tokio/index.html
* https://docs.rs/process-wrap/10.0.0/process_wrap/tokio/struct.ProcessGroupChild.html
* https://docs.rs/process-wrap/10.0.0/process_wrap/tokio/trait.ChildWrapper.html
* https://docs.rs/tokio/1.53.1/tokio/process/struct.Command.html
* https://docs.rs/tokio/1.53.1/tokio/process/struct.Child.html
* https://docs.rs/duct/1.1.2/duct/, .../struct.Expression.html, .../struct.Handle.html, .../struct.ReaderHandle.html
* https://docs.rs/command-group/5.0.1/command_group/
