# Status

Working, end to end. Progress is honest rather than flattering — the last two rows
are distinctions worth keeping.

| | |
|---|---|
| Kernel, domain, ports, all four adapters | complete |
| Toolset, agent loop, composition | complete |
| CLI and TUI | complete, and one binary: `nanus` is headless or interactive depending on how it is invoked |
| Live path (streaming, tool calls, results fed back) | **verified against the real API** |
| Interactive TUI | view layer tested headlessly; raw-mode input needs a real terminal |

Composing a harness is `compose(&config).await` for the adapters, then
`Pending::start()` outside the runtime for the kernel — the two phases exist because
`block_on` cannot be called from inside a runtime, and the split is enforced by types
rather than by remembering.

## Known limits

- **Live tool calling was verified by inspection, not replay.** The framing replayed in
  `live_wire.rs` follows the documented shape, and a live text run confirmed the request
  side, TLS, auth, and the reasoning passback rule. What no hermetic test can prove is
  that the live API's tool-call frames match the replay byte for byte; if they differ,
  [`crates/nanus-adapter-deepseek/src/wire.rs`](../crates/nanus-adapter-deepseek/src/wire.rs)
  is the only file involved.
- **The TUI has no automated end-to-end test.** Raw mode needs a real terminal, so key
  handling and the alternate screen are exercised by hand. The view layer is covered
  headlessly against ratatui's `TestBackend`, which is where the logic lives. The part
  that *is* checked automatically is the refusal: `ratatui::init` panics rather than
  returning when there is no terminal, so the terminal is verified before it is taken.
- **DeepSeek is the only provider.** The `LlmPort` seam is real and a second adapter
  would be a single file, but none exists yet.
- **The sandbox is reported, not OS-enforced.** `nanus-adapter-local` checks the working
  directory and returns the policy, but installs no OS-level confinement. See
  [SAFETY.md](../SAFETY.md).

## Where the research lives

The dependency research, the process-execution measurements, the raw crates.io responses
and the probe workspaces that produced them live in a separate repository —
**`antstanley/nanus-research`** (private). They are evidence rather than shipped code, and
carrying them here meant a clone of the product also fetched research scratch.

One report stays in this repository, because it documents the framework the kernel
*implements* rather than the research that led to it:
[`cordis-mechanisms-report.md`](../cordis-mechanisms-report.md).

## How composition is staged

`compose(&config).await` builds the adapters; `Pending::start()` mounts the kernel. The
two phases exist because `block_on` cannot be called from inside a runtime, and the
split is enforced by types — `Pending` is the seam that makes the ordering a compile-time
fact rather than a convention someone has to remember. Getting it wrong produced
"cannot start a runtime from within a runtime" on every command, which is how it was
found.
