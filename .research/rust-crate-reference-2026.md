# Rust crate ecosystem for a headless CLI + TUI agent harness — verified reference (Sep 2026)

Toolchain probed: **rustc 1.98.0 (88d9e12ae 2026-08-18)**, cargo 1.98.0, **cargo-nextest 0.9.126**,
aarch64-apple-darwin / macOS 26.6.2. Every version below was read from the crates.io JSON API
(`max_stable_version`), then **resolved, compiled and executed** in `.research/probe/` (a real
4-crate workspace). Verification artifacts: `probe/Cargo.lock`, `probe/core/tests/api_smoke.rs`
(17 tests), `probe/core/tests/groupkill_safe.rs`, `probe/core/tests/tls_live.rs`,
`probe/core/tests/processwrap.rs`, `probe/tui/tests/widget.rs`, `probe/tests/tests/proptest_demo.rs`.

**Result: 23 tests pass, 0 fail under `cargo nextest run --workspace`; `cargo build --workspace`
exits 0; the same workspace also `cargo check`s clean on the older Rust 1.94.0.**

---

## 1. Reference table — exact dependency lines known to resolve

Copy into the root virtual manifest under `[workspace.dependencies]`.

| # | Crate | Latest stable | Published | MSRV | Exact line |
|---|-------|---------------|-----------|------|-----------|
| 1 | ratatui | **0.30.2** | 2026-06-19 | 1.88.0 | `ratatui = { version = "0.30.2", default-features = false, features = ["crossterm","crossterm_0_29","all-widgets","layout-cache","macros","underline-color"] }` |
| 2 | crossterm | **0.29.0** | 2025-04-05 | 1.63.0 | `crossterm = { version = "0.29.0", features = ["event-stream"] }` |
| 3 | tokio | **1.53.1** | 2026-07-20 | 1.71 | `tokio = { version = "1.53.1", features = ["rt-multi-thread","macros","process","io-util","fs","time","sync","signal","net"] }` |
| 4 | reqwest | **0.13.5** | 2026-09-08 | 1.85.0 | `reqwest = { version = "0.13.5", default-features = false, features = ["rustls","json","http2","charset","stream","system-proxy","gzip","brotli","deflate","zstd"] }` |
| 5 | serde | **1.0.229** | 2026-07-18 | 1.56 | `serde = { version = "1.0.229", features = ["derive"] }` |
| 5 | serde_json | **1.0.151** | 2026-07-20 | 1.71 | `serde_json = "1.0.151"` |
| 6 | thiserror | **2.0.20** | 2026-08-08 | 1.71 | `thiserror = "2.0.20"` |
| 7 | clap | **4.6.6** | 2026-08-06 | 1.85 | `clap = { version = "4.6.6", features = ["derive","env","wrap_help"] }` |
| 8 | tracing | **0.1.44** | 2025-12-18 | 1.65.0 | `tracing = "0.1.44"` |
| 8 | tracing-subscriber | **0.3.23** | 2026-03-13 | 1.65.0 | `tracing-subscriber = { version = "0.3.23", features = ["env-filter","json","fmt","ansi"] }` |
| 9 | toml | **1.1.6+spec-1.1.0** | 2026-09-10 | 1.85 | `toml = "1.1.6"` (Cargo accepts the `1.1` req and ignores build metadata) |
| 10 | rustls | **0.23.44** | 2026-09-07 | 1.71 | `rustls = "0.23.44"` — **not needed directly if you only use reqwest** |
| 11 | proptest *(dev)* | **1.11.0** | 2026-03-24 | 1.85 | `proptest = "1.11.0"` |
| 12 | similar | **3.2.0** | 2026-08-17 | 1.85 | `similar = "3.2.0"` — **recommended** |
| 12 | diffy | 0.5.2 | 2026-08-31 | 1.85.0 | `diffy = "0.5.2"` — alternative |
| 13 | globset | **0.4.20** | 2026-08-04 | 1.88 | `globset = "0.4.20"` |
| 13 | ignore | **0.4.33** | 2026-08-04 | 1.88 | `ignore = "0.4.33"` — use **both** (see §13) |
| 14 | tempfile *(dev)* | **3.27.0** | 2026-03-11 | 1.63 | `tempfile = "3.27.0"` |
| 15 | process-wrap | **10.0.0** | 2026-08-24 | 1.87.0 | optional; **not required** — `tokio::process` + `nix` suffices |
| 15 | nix | **0.31.3** | 2026-05-11 | 1.69 | `nix = { version = "0.31.3", features = ["signal","process"] }` (unix; keeps `unsafe_code = "forbid"` clean) |
| 16 | reedline | **0.51.0** | 2026-08-22 | — | `reedline = "0.51"` — **only** for a non-ratatui REPL; TUI needs nothing extra |
| 16 | rustyline | 18.0.1 | 2026-06-24 | — | `rustyline = "18.0"` — alternative |
| 17 | uuid | **1.26.1** | 2026-09-10 | 1.85.0 | `uuid = { version = "1.26.1", features = ["v4","v7","serde"] }` — **recommended** |
| 17 | ulid | 3.0.0 | 2026-07-16 | — | `ulid = "3.0.0"` — see the monotonicity trap in §17 |
| 18 | etcetera | **0.11.0** | 2025-10-28 | 1.87.0 | `etcetera = "0.11.0"` — **recommended** |
| 18 | directories | 6.0.0 | 2025-01-12 | — | `directories = "6.0.0"` — stable but stale (last release Jan 2025) |

Split crates pulled in transitively by the ratatui facade (do **not** depend on these directly):
`ratatui-core 0.1.2`, `ratatui-widgets 0.3.2`, `ratatui-crossterm 0.1.2`, `ratatui-macros 0.7.2`.

Transitively resolved for reqwest+rustls: `hyper 1.11.1`, `hyper-rustls 0.27.9`,
`rustls-platform-verifier 0.7.0`, `aws-lc-rs 1.18.1`, `h2 0.4.19`, `tower-http 0.6.11`.

### Three version jumps that will break a 2024-era plan

1. **reqwest is 0.13, not 0.12.** `default-tls` now maps to **rustls** (`default = ["default-tls","charset","http2","system-proxy"]`, `default-tls = ["rustls"]`). **OpenSSL is no longer the default.** There is no bare `rustls` vs `native-tls` toggle to reason about; `native-tls` is now the opt-in.
2. **ratatui is 0.30**, with backends/widgets split into separate crates and `ratatui::run()` added.
3. **toml is 1.x** (`1.1.6+spec-1.1.0`), a real major bump from the 0.8 most projects pinned.

---

## 2. `ratatui` — TUI (0.30.2)

**Is crossterm the right backend? Yes, and no separate crate is needed.** With feature `crossterm`,
the facade re-exports the backend (`ratatui` 0.30.2 `src/lib.rs:504`):

```rust
pub mod backend {
    pub use ratatui_core::backend::{Backend, ClearType, TestBackend, WindowSize};
    pub use ratatui_crossterm::{CrosstermBackend, FromCrossterm, IntoCrossterm};
}
```

`ratatui-crossterm` **exists** (0.1.2) but is for backend-specific work. Use the facade.

**Yes, the API changed.** `ratatui::init()` and friends still exist *and* `run()` was added in 0.30.0.
Exact signatures from `ratatui-0.30.2/src/init.rs`:

```rust
pub type DefaultTerminal = Terminal<CrosstermBackend<Stdout>>;
pub fn run<F, R>(f: F) -> R where F: FnOnce(&mut DefaultTerminal) -> R;  // init, run, restore
pub fn init() -> DefaultTerminal;                       // panics; raw mode + alt screen + panic hook
pub fn try_init() -> io::Result<DefaultTerminal>;
pub fn init_with_options(o: TerminalOptions) -> DefaultTerminal;  // raw mode, NO alt screen
pub fn restore();  pub fn try_restore() -> io::Result<()>;
```

All are gated on `#[cfg(feature = "crossterm")]` — **with `default-features = false` and no
`crossterm` feature, `ratatui::init` and `ratatui::backend::CrosstermBackend` do not exist.**

Idiomatic loop (`probe/tui/src/app.rs`, compiles + renders headlessly): draw first, then
`event::poll(tick)`, then drain with `poll(Duration::ZERO)`.

```rust
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
let mut terminal = ratatui::init();
while !app.should_quit {
    terminal.draw(|f| app.draw(f))?;
    if event::poll(Duration::from_millis(100))? {
        loop {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press { /* handle */ }
            }
            if !event::poll(Duration::ZERO)? { break; }
        }
    }
}
ratatui::restore();
```

**`ratatui` re-exports `crossterm`**: `#[cfg(feature = "crossterm")] pub use ratatui_crossterm::crossterm;`
→ `ratatui::crossterm::event::{poll, read, ...}` and `ratatui::crossterm::execute!` work with **no
direct crossterm dependency**. The one exception: **`EventStream` needs a direct `crossterm` dep**
(the facade has no passthrough for `event-stream`). Keep `crossterm = "0.29.0"` if you want the async
`EventStream` path. **Verified in this workspace: exactly one crossterm 0.29.0 in the graph**, shared
by the app and `ratatui-crossterm` (`cargo tree -i crossterm`).

**Traps (independently found, worth avoiding):**
- **`crossterm_0_28` on the facade is a trap.** It does not select crossterm 0.28; because
  `ratatui-crossterm` turns on its own defaults, crossterm **0.29** still binds, both majors land in
  the graph and you get `E0308: there are multiple different versions of crate 'crossterm'`.
  Just use `crossterm` / `crossterm_0_29` and pin the direct dep to 0.29.
- `widgets::block::*` and `widgets::borders::*` **module paths are gone** in 0.30 (items moved up,
  `Title` deleted). `widgets::Block`, `widgets::Borders` etc. still work.
- `Backend` gained an associated `type Error`; `TestBackend`'s is now `core::convert::Infallible`.
  Generic `fn f<B: Backend>(t: Terminal<B>) -> io::Result<()>` no longer compiles — use
  `B::Error`, or just take `DefaultTerminal`.
- crossterm↔ratatui colour conversion now needs `FromCrossterm`/`IntoCrossterm`.
- `Frame::size()` is deprecated → use `Frame::area()`.
- With `default-features = false`, **`layout-cache` is off** — re-enable it (it is in the line above).

**Headless widget testing still works** and needs no extra feature:
`use ratatui::backend::TestBackend;` (`TestBackend::new(60,12)`, `Terminal::new(..)`,
`terminal.backend().assert_buffer_lines([...])`). Verified in `probe/tui/tests/widget.rs`.

---

## 3. `crossterm` (0.29.0)

Depend on it **only** for `event-stream` (async `EventStream`). `poll`/`read` are not feature-gated
and are re-exported by ratatui. Default features are `["bracketed-paste","events","windows","derive-more"]`.
In 0.29 `KeyEvent` carries `kind` — filter to `KeyEventKind::Press` or you will handle
press **and release** for every keystroke (a classic duplicate-input bug).

---

## 4. `tokio` (1.53.1)

Latest 1.x. `full` exists but the explicit feature list above is leaner and sufficient. Note
`io-util` is what gives you `AsyncBufReadExt::lines()` for streaming child output.

---

## 5. `reqwest` (0.13.5) — TLS + JSON with rustls, no OpenSSL

**Recommended features:**

```toml
reqwest = { version = "0.13.5", default-features = false, features = [
  "rustls", "json", "http2", "charset", "stream", "system-proxy",
  "gzip", "brotli", "deflate", "zstd",
] }
```

- `json` is **not** in default features — without it, `.json()` does not exist. Easy to miss.
- `rustls` pulls `__rustls-aws-lc-rs` (aws-lc-rs provider) + `rustls-platform-verifier` (uses the OS
  trust store — the right choice on macOS).
- `stream` is needed for SSE-style streaming token responses.
- `system-proxy` keeps default proxy behaviour if you go `default-features = false`.
- **Do not add `native-tls`**; that is the only path that can reach OpenSSL.

**Do you need explicit rustls config? No.** I verified this live rather than assuming: with
`reqwest = { features = ["rustls"] }` and **no** `CryptoProvider::install_default()` call anywhere,
`GET https://index.crates.io/config.json` returned **200** and `GET https://api.deepseek.com/models`
returned **401 Unauthorized** (i.e. TLS + HTTP/2 + routing all succeeded; only auth was missing).
So the widely-copied advice to call
`rustls::crypto::aws_lc_rs::default_provider().install_default()` in `main()` is **unnecessary here**
(and returns `Err` if already installed). You do **not** need a direct `rustls` dependency for reqwest.
Keep `rustls = "0.23.44"` only if your own code manipulates TLS types.

Note the build cost: the default provider is **aws-lc-rs**, which compiles C/assembly. If you want a
pure-Rust provider instead, use `rustls-no-provider` plus an explicit `ring` provider and install it
yourself — that is the one case where manual provider setup *is* required.

DeepSeek specifics: OpenAI-compatible, so `POST https://api.deepseek.com/chat/completions` with
`.bearer_auth(key)`, body `{"model":"deepseek-chat"|"deepseek-reasoner","messages":[...],"stream":true}`.
Verified the request builds with `Authorization` and `Content-Type: application/json` headers set.

---

## 6. `serde` / `serde_json`

Unchanged API. `serde = { version = "1.0.229", features = ["derive"] }`, `serde_json = "1.0.151"`.
serde still 1.x (no 2.0). Verified `json!`, `to_string`, `from_str`, and `Value` indexing.

---

## 7. `thiserror` — **v2** (2.0.20)

Current major is **2**. MSRV 1.71. Derive syntax is compatible with v1 for ordinary use
(`#[derive(Error)]`, `#[error("...")]`, `#[from]`, `#[source]`); the v2 changes that matter are
that the blanket `From` for `Box<dyn Error>` handling was reworked and `thiserror::Error` no longer
generates an implicit `From` where you did not ask for one. Verified compiling an enum with
`#[from]` for `std::io::Error`, `toml::de::Error`, `serde_json::Error`, `reqwest::Error`, plus a
struct variant with an explicit `#[source]`. `default = ["std"]`.

*(Side note: `probe/Cargo.lock` also contains thiserror **1.0.69**, pulled only by ratatui's optional
`termwiz` backend path. It is resolved but never compiled with our features — harmless, not a conflict.)*

---

## 8. `clap` (4.6.6)

Latest 4.x, derive unchanged. Verified compiling and **running** a `Parser` + `Subcommand` app with
`#[arg(long, global = true, env = "NANUS_CONFIG")]`, `ArgAction::Count` for `-v/-vv`, `default_value`,
and workspaced `version`/`about`. `--help` output rendered correctly. Features: add `derive`;
`env` is needed for `env = ...`; `wrap_help` improves long help text.

## 9. `tracing` / `tracing-subscriber`

`tracing 0.1.44`, `tracing-subscriber 0.3.23`. Verified `fmt()` + `EnvFilter::try_new("info,nanus=debug")`
+ `with_target(false)` compiles and emits. Add `json` feature for structured logs; use
`EnvFilter` (feature `env-filter`) so `RUST_LOG` works.

## 10. `toml` — now **1.x**

`toml = "1.1.6"`. The classic API survived the major bump: `toml::from_str`, `toml::to_string`, and
`toml::Value` with `.get(..)`/`as_str()` all verified working round-trip. (The version string carries
`+spec-1.1.0` build metadata, which a plain `"1.1.6"` requirement matches fine.)

---

## 11. `rustls` (0.23.44)

Still 0.23 (no 0.24). **Not needed as a direct dependency** for a reqwest-only harness — see §5.
Default features are `["aws_lc_rs","logging","prefer-post-quantum","std","tls12"]`.

---

## 12. `proptest` (1.11.0, dev-dependency)

Verified via `proptest!` macro with `Vec<u8>` inputs, ranged strategies (`max in 0usize..64`),
`prop_assert!`/`prop_assert_eq!`, regex string strategies (`"\\PC*"`), and the manual
`TestRunner::new(Config { cases: 32, .. })` form. All 4 tests pass under nextest.
`default = ["std","fork","timeout","bit-set"]` — note **`fork` is a default**, i.e. proptest forks a
child process per case by default; disable it (`default-features = false, features = ["std"]`) if you
run in a sandbox that forbids `fork`.

---

## 13. `similar` vs `diffy` — **use `similar`**

| | `similar` 3.2.0 | `diffy` 0.5.2 |
|---|---|---|
| Unified diff | `TextDiff::from_lines(a,b).unified_diff().context_radius(3).header("a","b").to_string()` ✅ verified | `create_patch(a,b).to_string()` ✅ verified |
| Apply a patch | not the focus | `diffy::apply(orig, &patch)` ✅ verified |
| Line/char/word/inline diff, iterating changes | rich: `iter_all_changes()`, `ChangeTag::{Equal,Insert,Delete}` ✅ verified | narrower |
| Algorithm control | `Algorithm::Myers`/`Patience`/`Lcs` | Myers only |
| Maintainer | mitsuhiko (insta/askama ecosystem) | bmwill (jj ecosystem) |

**Recommendation: `similar`**, because a file-edit preview tool wants the *changes* (per-hunk, per-line,
with tags to colour them) more than it wants patch application, and `similar` exposes that plus a
unified-diff renderer. Add `diffy` only if you need to *apply* patches (e.g. reverting an agent edit).
Both are equally well maintained, so this is a close call decided by API fit.

---

## 14. `globset` vs `ignore` — use **both**, they are different jobs

- `globset` — "does this path match this pattern?" Compile a `GlobSet` once, match cheaply.
  Verified: `Glob::new("src/**/*.rs")` + `GlobSetBuilder`, `set.is_match("src/main.rs")` == true,
  `is_match("src/main.py")` == false. This is what a `glob`/`grep` **tool parameter** needs.
- `ignore` — "walk a directory tree, honouring `.gitignore`/.ignore/hidden rules, in parallel."
  Verified: `WalkBuilder::new(dir).hidden(false).git_ignore(false).build()`.
  This is what a **file-search tool** needs so it does not return `target/` and `node_modules/`.
- MSRV note: both jumped to **1.88** (`globset 0.4.20` / `ignore 0.4.33`, 2026-08-04). Fine on 1.98.

Use `ignore`'s walker for traversal and `globset` for user-supplied include/exclude patterns
(`WalkBuilder::filter_entry` / `overrides`).

---

## 15. `tempfile` (3.27.0, dev-dependency)

`default = ["getrandom"]`. Verified `tempfile::tempdir()` for tests that touch the filesystem.

---

## 16. Subprocess execution — **recommendation: `tokio::process` directly, plus `nix`**

Full detail in `.research/subprocess-runner-report.md`. The decisive question for an agent harness is
not streaming, it is **orphaned processes**, because a harness runs `sh -c "..."` — the shell is the
direct child and every real tool (`cargo`, `npm`, `make`, pipelines) is a **grandchild**.

Measured on this machine (spawn `sh -c 'sleep 300 & echo $! > pid; wait'`, then check grandchild liveness):

| Action | Grandchild survives? |
|---|---|
| `Child::kill().await` (no process group) | **YES — orphaned** |
| `kill_on_drop(true)` then drop the `Child` | **YES — orphaned** |
| `process_group(0)` + `killpg(pgid, SIGKILL)` | no ✅ |
| `process-wrap` `ProcessGroup` + `.kill()` | no ✅ |

**I reproduced this independently** in `probe/core/tests/groupkill_safe.rs`: with
`process_group(0)` + `killpg`, the grandchild is dead (`SAFE_NIX_GROUP_KILL grandchild_alive=false`).
So `kill_on_drop` is **not** sufficient and is a trap that looks safe.

Verdicts on the crates:
- **`tokio::process` — recommended.** `Command::process_group(0)` is an *inherent* Unix method in tokio
  1.53 (you do **not** need `std::os::unix::process::CommandExt`), and `kill_on_drop(bool)` exists.
  There is no built-in group kill, so pair it with `nix::sys::signal::killpg` (safe API → keeps
  `unsafe_code = "forbid"` intact). `Child::wait()` is cancel-safe, so
  `tokio::time::timeout(d, child.wait())` then `killpg` works.
- **`process-wrap` 10.0.0 — viable but unnecessary.** Actively maintained successor to
  `command-group`; feature name is **`tokio1`** (not `tokio`). Its `KillOnDrop` is just a shim over
  `kill_on_drop(true)` and therefore does **not** group-kill on drop; `ProcessGroup` + explicit
  `.kill()` does. It has **no `Timeout` and no `Resize` wrapper** in 10.0.0. Verified compiling and
  running:
  `CommandWrap::with_new("sh", |c| { c.arg("-c").arg("..."); }).wrap(ProcessGroup::leader()).spawn()?`
  then `child.start_kill()?`. Take it only if you want its zombie-free group-reap loop.
- **`command-group` 5.0.1 — do not use.** Last release 2023-11-18; the project announced succession
  to `process-wrap`. I removed it from the probe workspace; it was the sole reason a stale `nix 0.27`
  stayed in the lock graph.
- **`duct` 1.1.2 — not suitable.** Sync/blocking (needs `spawn_blocking` in tokio), and it never calls
  `setpgid`, so it cannot group-kill. `Read`/`Handle::kill()` do not reap grandchildren. `.read()`
  captures at end; only `.reader()` streams.

**Two non-obvious hazards for the streaming path:**
1. **`AsyncReadExt::take(n)` on a pipe HANGS, it does not truncate.** Once you stop reading, the writer
   blocks on a full 64 KiB pipe and the child never exits. Correct shape: read to EOF, keep at most
   `cap` bytes (ring/tail buffer), set `truncated = true` — and cap stdout and stderr **separately**.
2. **Bound the live-output channel too.** An unbounded `mpsc` accumulated millions of events from a
   50 MB producer. Use `mpsc::channel(N)` + `try_send` and coalesce/drop on `Full`.

Minimal safe runner shape (verified compiling and executing):

```rust
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

pub struct Outcome { pub code: Option<i32>, pub stdout: Vec<u8>, pub stderr: Vec<u8>,
                     pub truncated: bool, pub timed_out: bool }

pub async fn run_shell(script: &str, timeout: Duration, cap: usize) -> std::io::Result<Outcome> {
    let mut child = Command::new("sh")
        .arg("-c").arg(script)
        .process_group(0)          // unix: new process group, pgid == child pid
        .kill_on_drop(true)        // belt-and-braces for the direct child only
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let pgid = nix::unistd::Pid::from_raw(child.id().expect("pid") as i32);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut truncated = false;

    let mut so = BufReader::new(child.stdout.take().expect("stdout"));
    let mut se = BufReader::new(child.stderr.take().expect("stderr"));

    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    let mut obuf = [0u8; 8192];
    let mut ebuf = [0u8; 8192];
    let mut timed_out = false;
    loop {
        tokio::select! {
            n = so.read(&mut obuf) => {
                let n = n?;
                if n == 0 { break; }
                push_capped(&mut out, &obuf[..n], cap, &mut truncated);
            }
            n = se.read(&mut ebuf) => {
                let n = n?;
                if n == 0 { continue; }
                push_capped(&mut err, &ebuf[..n], cap, &mut truncated);
            }
            () = &mut deadline => {
                // kill the WHOLE group; plain child.kill() would orphan grandchildren
                let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
                timed_out = true;
                break;
            }
        }
    }
    let status = child.wait().await?;
    Ok(Outcome { code: status.code(), stdout: out, stderr: err, truncated, timed_out })
}

fn push_capped(dst: &mut Vec<u8>, chunk: &[u8], cap: usize, truncated: &mut bool) {
    dst.extend_from_slice(chunk);
    if dst.len() > cap {
        let excess = dst.len() - cap;
        dst.drain(..excess);   // keep the TAIL, which is what you want for errors
        *truncated = true;
    }
}
```

(Needs `tokio` features `process`,`io-util`,`time`,`macros` and `nix` with `signal`+`process`.
This is a simplified illustration — for real concurrency, feed a bounded `mpsc` from two spawned
reader tasks instead of one `select!` loop.)

---

## 17. Line editing — **ratatui covers it for the TUI; nothing extra needed**

If you build the interactive prompt as a ratatui widget (the normal choice), input editing, cursor
movement and history are your own `String` + `KeyCode` handling — no line-editing crate required, and
adding one would fight ratatui for the terminal. Only reach for a dedicated crate if you want a
**plain readline REPL outside the TUI**:
- `reedline 0.51.0` (2026-08-22) — actively developed, nushell's line editor; better maintained.
- `rustyline 18.0.1` (2026-06-24) — the long-standing classic.

For the headless/non-interactive CLI path (piped stdin), use `tokio::io::stdin` + `BufRead::lines()`
and no crate at all.

---

## 18. Session IDs — **`uuid` 1.26.1 with `v7`, recommended**

`uuid = { version = "1.26.1", features = ["v4","v7","serde"] }`. Verified `Uuid::new_v4()`,
`Uuid::now_v7()`, `get_version_num()`, `parse_str`, and serde round-trip. **Prefer `v7` for session
ids**: it is time-ordered (so ids sort by creation and index well) while remaining globally unique.
`v4` is fine if you never sort by id. Both features are independent.

**ULID trap (measured).** `ulid = "3.0.0"` is current, but `Ulid::generate()` does **not** guarantee
sort order — its own source says *"Using this function to generate Ulids will not guarantee monotonic
sort order"*, because the low bits are random within a millisecond. A naive
`assert!(Ulid::generate() <= Ulid::generate())` test **failed 2 times in 20 runs**. If you want
ULIDs, use the monotonic generator (`ulid::Generator::new()` → `.generate()`), which increments the
low bits within the same millisecond (verified `x < y` with equal `timestamp_ms()`, 20/20 stable).
Also note: **`gen` is a reserved keyword in edition 2024**, so `let mut gen = ...` does not compile.
Given a choice, **`uuid` v7 is the lower-surprise option**; pick ULID only if you specifically want
26-char Crockford base32.

---

## 19. Config directories — **`etcetera` 0.11.0, recommended**

Both verified working side by side. `etcetera` is the actively maintained one (Oct 2025, MSRV 1.87)
and is explicit about strategy:

```rust
use etcetera::BaseStrategy;
let strategy = etcetera::choose_base_strategy()?;   // picks XDG vs Apple vs Windows
let cfg = strategy.config_dir();                    // ~/Library/Application Support on macOS
```

`directories 6.0.0` still works (`ProjectDirs::from("dev","nanus","nanus")`) but has not been
released since **Jan 2025**. Use `etcetera` for new code; `ProjectDirs` is nicer only if you want the
org/app path nesting without extra work. Neither is a hard dependency — both are thin wrappers over
env vars.

---

## 20. `#![forbid(unsafe_code)]` — safe to use, and it does **not** affect dependencies

**Confirmed, including by experiment.** The `unsafe_code` lint is a *local* lint: it is evaluated
against the source of the crate being compiled. Dependencies are compiled as their own crates with
their own lint settings, so `tokio`, `ratatui`, `reqwest`, `rustls`, `aws-lc-rs` etc. are entirely
unaffected by your `forbid`. This whole probe workspace sets `unsafe_code = "forbid"` and builds
cleanly with those crates.

`forbid` is strictly stronger than `deny`: it makes the lint an error **and removes the ability to
override it** — an inner `#[allow(unsafe_code)]` is rejected (`E0453`). I hit this for real: a test
of mine using `libc::killpg` failed to compile with *"usage of an `unsafe` block"*, and adding
`#![allow(unsafe_code)]` at the top of that file produced four more `E0453` errors. **This is the
argument for using `nix`'s safe wrappers** in the process-group runner (§16) rather than raw `libc` —
it keeps the guarantee intact instead of forcing you to weaken it.

`[lints.rust] unsafe_code = "forbid"` in `Cargo.toml` and the `#![forbid(unsafe_code)]` attribute are
equivalent in force, but the **manifest form has strictly broader coverage**. Verified by experiment:

| target | `[lints.rust] unsafe_code="forbid"` | `#![forbid(unsafe_code)]` in `src/lib.rs` |
|---|---|---|
| lib / bins / tests / benches / examples | yes | only the file it is written in |
| **build script (`build.rs`)** | **yes** | no (separate crate) |
| **doctests** | **NO** | **NO** |

- **Doctests are the coverage hole**, and I confirmed it directly: a workspace with
  `unsafe_code = "forbid"` plus a doctest containing `unsafe { std::ptr::read_volatile(&1u8) };`
  compiles and **passes** (`cargo test --doc` → 2 passed). Cargo does pass the flag to the
  `rustdoc --test` invocation, but rustdoc resets lint levels for the synthesised doctest crate.
  Practical rule: **use both** the manifest lint *and* the crate-root attribute, and never assume
  doctests are lint-covered.
- **`[lints] workspace = true` is all-or-nothing.** A member that opts in *and* also declares a local
  `[lints.rust]` is a hard error. And opting in when the root has no `[workspace.lints]` is a hard
  error too — I reproduced it verbatim: *"error inheriting `lints` from workspace root manifest's
  `workspace.lints` / `workspace.lints` was not defined"*. Put **everything** in the root.
- Two real limitations of `forbid` worth knowing before you commit to it:
  1. `unsafe_code` also fires on `#[no_mangle]`, `#[export_name]`, `#[link_section]`,
     `unsafe trait`/`unsafe impl`, `global_asm!` and `unsafe extern` blocks — and under `forbid` you
     **cannot** `#[allow]` them. If an adapter ever needs FFI, that member must use a local
     `unsafe_code = "deny"`, because a workspace-wide `forbid` cannot be relaxed per crate.
  2. `unsafe` emitted by a **dependency's proc macro** (e.g. a `#[derive]` that generates unsafe
     internals) is **not** reported. So `forbid` is a strong local guarantee but not a proof that no
     `unsafe` is compiled into your binary.


---

## 21. Recommended `[workspace]` layout (hexagonal)

A **virtual manifest** at the root (no `[package]`, so `cargo` commands act on the workspace), a
`crates/` directory, shared metadata and dependency versions centralised, and lints inherited by
every member. This layout is validated by the probe workspace, which builds and tests green.

```
nanus/
├── Cargo.toml                 # virtual manifest: members, [workspace.*], [profile.*], [patch.*]
├── Cargo.lock                 # committed (this is an application, not a library)
├── rust-toolchain.toml        # pin channel = "1.98.0" for reproducibility
├── .config/nextest.toml       # nextest config (see §22)
├── crates/
│   ├── nanus-domain/          # PURE domain: entities, value objects, ports (traits). No I/O deps.
│   ├── nanus-app/             # use-cases / orchestration; depends on domain only
│   ├── nanus-ports/           # (optional) port traits if you want domain fully dep-free
│   ├── nanus-adapters/        # driven adapters: DeepSeek HTTP, fs, subprocess, mcp clients
│   ├── nanus-config/          # config load/merge/validate (toml, etcetera)
│   ├── nanus-cli/             # [[bin]] nanus      -> clap, wires adapters into app
│   └── nanus-tui/             # [[bin]] nanus-tui  -> ratatui, wires the SAME app
└── tests/                     # (optional) workspace-level integration tests
```

Rules that keep it honest:
- **Dependencies point inward.** `domain` depends on nothing but `serde`/`thiserror`. `app` depends on
  domain + port traits. `adapters` implement the ports and are the only crates that know about
  `reqwest`, `tokio::process`, `ignore`. The binaries depend on everything and contain no logic.
- **Two binaries share one app core** — the CLI and TUI must not duplicate agent logic. This is the
  single biggest layout mistake to avoid.
- `nanus-cli` and `nanus-tui` set `publish = false`.
- Feature flags for optional adapters live on the adapter crate, not on `domain`.

Root manifest skeleton (exactly what the probe uses, verified):

```toml
[workspace]
resolver = "3"                     # edition-2024 default; MUST be stated explicitly in a virtual
                                   # manifest. Changes exactly one thing vs resolver 2: MSRV-aware
                                   # dependency resolution.
members = ["crates/*"]             # globs are NOT allowed in default-members -- list paths explicitly
default-members = ["crates/nanus-cli", "crates/nanus-tui"]
exclude = []                       # path deps inside the workspace dir auto-become members; exclude them here

[workspace.package]
edition = "2024"                   # NOTE: makes `gen` a reserved keyword
rust-version = "1.88"              # driven by ratatui 0.30.2's MSRV
license = "MIT OR Apache-2.0"
repository = "https://github.com/you/nanus"

[workspace.dependencies]           # versions live HERE, once
ratatui = { version = "0.30.2", default-features = false, features = ["crossterm","crossterm_0_29","all-widgets","layout-cache","macros","underline-color"] }
tokio   = { version = "1.53.1", features = ["rt-multi-thread","macros","process","io-util","fs","time","sync","signal","net"] }
# ...all rows from §1...

# Profiles are legal in a *virtual* root and apply workspace-wide. Verified accepted by cargo 1.98.
[profile.release]
lto = "thin"
codegen-units = 1
strip = "symbols"
panic = "abort"                    # safe for a CLI/TUI; remove if you need catch_unwind across FFI

[profile.dev]
debug = 1

[profile.test]
opt-level = 1                      # proptest and integration tests run much faster
```

Each member then opts in with no version numbers:

```toml
[package]
name = "nanus-cli"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[lib]                              # optional: keep logic in lib.rs, main.rs stays a shim
[[bin]]
name = "nanus"
path = "src/main.rs"

[dependencies]
nanus-app = { path = "../nanus-app" }
tokio = { workspace = true }
clap  = { workspace = true }

[lints]
workspace = true                   # REQUIRED — inheriting is opt-in per crate

[dev-dependencies]
proptest = { workspace = true }
```

### `[workspace.lints]` for clippy pedantic + `unsafe_code = "forbid"`

```toml
[workspace.lints.rust]
unsafe_code = "forbid"                 # enforced proof: rejects unsafe in local code, incl. tests
# Groups MUST carry a negative priority too, or you get a lint_groups_priority warning:
rust_2018_idioms = { level = "warn", priority = -1 }
missing_debug_implementations = "warn"
unused_lifetimes = "warn"

[workspace.lints.rustdoc]
broken_intra_doc_links = "deny"
missing_crate_level_docs = "warn"

[workspace.lints.clippy]
# priority: LOWER number wins when two groups set the same lint.
all      = { level = "warn", priority = -1 }
pedantic = { level = "warn", priority = -1 }
nursery  = { level = "warn", priority = -1 }

# ...then re-allow the genuinely noisy pedantic lints (higher priority than -1):
must_use_candidate      = "allow"
missing_errors_doc      = "allow"
missing_panics_doc      = "allow"
module_name_repetitions = "allow"
cast_precision_loss     = "allow"
too_many_lines          = "allow"

# Project policy worth making a hard error in an agent harness:
unwrap_used = "deny"
expect_used = "warn"
panic       = "warn"
```

`priority` semantics (Cargo Book): *"lower (particularly negative) numbers have lower priority, being
overridden by higher numbers, and show up first on the command-line"*. Cargo emits low-priority
entries first and rustc is last-wins, so pinning the broad **groups** to `-1` lets individual entries
at the implicit `0` be emitted last and therefore win. Measured: with `all`/`pedantic`/`module_name_repetitions`
all at priority 0, the individual `allow` was **defeated** (3 warnings); with the groups at `-1` and
the individual entry left at 0, the `allow` **wins** (1 warning). Order in the table is irrelevant —
`clippy::lint_groups_priority` exists precisely to catch same-priority groups, and it does **not**
check lints inherited via `lints.workspace = true`, so get this right in the root manifest.

Verified in the probe: `cargo clippy` reports `clippy::pedantic`, `clippy::unwrap_used`,
`clippy::expect_used`, `clippy::missing_const_for_fn`, `clippy::doc_markdown`,
`clippy::cast_possible_wrap`, `clippy::collapsible_if` — i.e. the workspace config really is reaching
member crates.

Note `clippy` is not a dependency of `cargo build`; these lints appear under `cargo clippy`, and
`unsafe_code` appears under both.

**Escape hatch for MSRV-aware resolution** (useful if resolver 3 ever picks an unexpectedly old
dependency version for a member with a low `rust-version`), in `.cargo/config.toml`:

```toml
[resolver]
incompatible-rust-versions = "allow"   # restore pre-resolver-3 behaviour
```

---

## 22. `.config/nextest.toml` — complete working config for nextest 0.9.126

Validated against the installed binary with `cargo nextest show-config test-groups`, which parsed the
file and correctly reported the override/groups below. The file lives at **`.config/nextest.toml`**
in the workspace root.

```toml
[store]
dir = "target/nextest"

nextest-version = { required = "0.9.126" }   # fail loudly if CI has a different nextest

[test-groups.serial-heavy]
max-threads = 1                       # tests that must not run concurrently

[profile.default]
default-filter = "not test(/requires_network/)"          # profile-level key; see note below
retries = { backoff = "exponential", count = 2, delay = "250ms", max-delay = "5s", jitter = true }
fail-fast = { max-fail = 10, terminate = "wait" }         # or plain `true`/`false`
test-threads = "num-cpus"                                 # int | negative int | "num-cpus"
slow-timeout = { period = "60s", terminate-after = 3, grace-period = "10s", on-timeout = "fail" }
leak-timeout = "200ms"
global-timeout = "30m"                # don't let a wedged run hang CI forever
status-level = "pass"
final-status-level = "flaky"
failure-output = "immediate-final"
success-output = "never"

# Run network tests only when explicitly selected; they get their own timeout.
[[profile.default.overrides]]
filter = "test(/tls_live/)"
slow-timeout = { period = "30s", terminate-after = 2 }

# Force genuinely serial tests into a 1-thread group.
[[profile.default.overrides]]
filter = "test(/group_kill|groupkill/)"
test-group = "serial-heavy"
slow-timeout = { period = "20s", terminate-after = 2 }

# Marking a test SLOW in nextest 0.9.x = giving it a longer slow-timeout via an override.
[[profile.default.overrides]]
filter = "test(/large_repo|benchmark/)"
slow-timeout = { period = "300s", terminate-after = 1 }

[profile.ci]
inherits = "default"
retries = 0                           # do not mask flakiness in CI
fail-fast = true
status-level = "all"
final-status-level = "all"
slow-timeout = { period = "30s", terminate-after = 4 }

[profile.default.junit]
path = "junit.xml"                    # written to <store.dir>/<profile>/, NOT the cwd
store-success-output = false
store-failure-output = true

[profile.ci.junit]
path = "junit.xml"
report-name = "ci"
```

To make that opt-out work, name network tests so they match, e.g. `fn tls_live_requires_network()`.
`default-filter` is a **profile-level** key: an override may use `filter` **or** `default-filter`,
never both (see below).

Key points, all checked against the installed 0.9.126:

- **`retries`** accepts an integer (`retries = 2`) or the structured form shown. The structured form
  gives exponential backoff with jitter. Flaky tests show as `FLAKY` rather than `PASS`.
- **`slow-timeout`** is `{ period, terminate-after }`: after the first `period` the test is reported
  SLOW; after `period * terminate-after` it is terminated and failed. Note this is a *multiplicative*
  grace, not an absolute deadline.
- **Marking a test slow** — in 0.9.x there is no `slow-tests` list. You either tune `slow-timeout`
  per-test via an override (shown above), or annotate the test with the `nextest` attribute from the
  `nextest-tests` crate. `#[ignore]` is for *skipping*, not for marking slow.
- **`default-filter`** is a *profile-level* key. An override may use `filter` **or** `default-filter`,
  **not both** — I hit and fixed exactly that parse error
  (*"at most one of `filter` and `default-filter` must be specified"*).
- Overrides are **first-match** style per key; order matters, put specific before general.
- **There is no doctest support in 0.9.126.** `cargo nextest run --doctests` is rejected
  (*"unexpected argument '--doctests'"*, help suggests `--tests`). Run doctests separately:
  `cargo test --doc`. Do not plan around a `--doctests` flag on this version. (The installed binary
  is also ~7 months behind upstream — 0.9.126 / 2026-02-04 vs 0.9.144 / 2026-09-10 — and nexte.st
  serves docs for *main*, so check flags against the local `--help`, not the website.)
- **`default-members` + nextest is a trap I hit.** With `default-members` set (so bare `cargo build`
  builds only your binaries), bare **`cargo nextest run` selects those members, finds 0 tests, prints
  `error: no tests to run` and exits 4.** Always pass `--workspace` in CI, or add
  `--no-tests pass` (or `fail`) to control the empty-case exit code.
- **Validating the config:** `cargo nextest show-config test-groups` is the reliable check — it parses
  the whole file and hard-errors on bad values. `show-config version` does **not** parse profiles (it
  happily accepted a file with `slow-timeout = "banana"`). In 0.9.126 `show-config` has only the
  `version` and `test-groups` subcommands. Unknown keys are **non-fatal warnings**, not errors, so
  typos can silently do nothing — hence the explicit validation step.
- `#[should_panic]` and `proptest` both work under nextest (verified: proptest's 4 cases pass, and
  nextest runs each test in its own process, so proptest's forking does not conflict). proptest's
  default `fork` feature spawns children — fine locally, may be blocked in a sandbox; drop to
  `proptest = { version = "1", default-features = false, features = ["std","bit-set"] }` if needed.
- nextest re-runs a retried test in a **fresh process**, so process-local state never survives a retry.

Commands:

```bash
cargo nextest run --workspace                 # default profile
cargo nextest run --profile ci --workspace    # CI profile
cargo nextest run -E 'test(ulid_ids)'         # filter expression
cargo nextest run -- --nocapture              # pass args to the test binary
cargo nextest show-config test-groups         # validate config + inspect groups
cargo test --doc                              # doctests, since nextest 0.9.126 can't
```

---

## 23. What was verified vs. merely read

**Compiled and executed on rustc 1.98.0** (23 tests pass under nextest):
toml round-trip; serde_json; reqwest 0.13 rustls client + request build; **live HTTPS 200 from
index.crates.io and 401 from api.deepseek.com with no manual crypto-provider install**; globset
match/no-match; ignore walker; similar unified diff + change iteration; diffy patch + apply; uuid v4
and v7 (+ serde); ulid parse/serialize and monotonic `Generator`; etcetera + directories;
duct capture; tokio streaming lines with timeout; tokio timeout + kill_on_drop; truncation helper;
tracing subscriber; clap derive parse and `--help`; ratatui headless `TestBackend` render; proptest
strategies; **process-group kill semantics (grandchild death)**; process-wrap `ProcessGroup`; and
clippy/lint enforcement plus nextest config validation.

Also verified by direct experiment: the same workspace `cargo check`s clean on **Rust 1.94.0**
(so the stated MSRVs are not knife-edge for this toolchain); `unsafe_code = "forbid"` rejects an
inner `#[allow(unsafe_code)]` with `E0453`; **doctests escape the lint** (`unsafe` in a doctest passes
under `forbid`); `[lints] workspace = true` with no `[workspace.lints]` is a **hard error**;
`default-members` + bare `cargo nextest run` yields **"no tests to run", exit 4**; and
`cargo clippy` fires the configured pedantic lints on member crates.

**Read from primary sources but not executed:** ratatui's `init_with_options`/`try_*` variants, the
alternate backends (termina/termion/termwiz), and the `crossterm_0_28` trap (reported from a
compile-probed sub-investigation, not re-run here). `readline` crates were version-checked only.
No interactive TTY run was performed (`enable_raw_mode()` needs a real terminal), so live key
handling and the alternate screen are unverified — rendering is covered headlessly instead.
Setup scripts, JUnit output, `--partition`, and nextest's `--stress-*` were schema-checked only.

### Hardening notes / things that bit me

- The crates.io web API returns **HTTP 403 to a plain curl User-Agent**; it needs a custom UA
  (`-A "nanus-research/1.0"`). Make this explicit in any version-checking script.
- `crates.io/api/v1/crates/<name>` returns `versions` **newest-first** (my first timeline script had
  it backwards); `max_stable_version` is the field to trust.
- A `#[test]` asserting ULID ordering was genuinely flaky (2/20) — a good argument for the
  nextest `retries` config above, but better to fix the test.
- `gen` is reserved in edition 2024.
- `thiserror` 1.x appearing in `Cargo.lock` alongside 2.x is not a conflict; it is an optional
  dependency of ratatui's `termwiz` backend that is never built.
