# Nanus response-limit implementation review — 2026-10-02

**Scope:** the pre-event transport seam in the [embedding change](2026-09-30-library_embedding_and_multimodal_results.md). This is local unpublished work; text-model ceilings remain open. Later exact-protocol progress is recorded in
[its certificate](2026-10-02-exact_protocol.review.md). No Hype runtime, skill/plugin loader, deadline policy or credential fallback is introduced.

## Premises

P1. A consumer's `LlmEvent` guard runs after Nanus has buffered/decoded provider data. It cannot bound an unterminated SSE line or an HTTP error body read with `Response::text`.

P2. Optional immutable `ResponseLimits` must check prospective logical lengths/counts before owned copies/extensions and before JSON parsing, fail once, discard retained partial calls/replay and release the response. The host retains cancellation/deadline/record policy.

P3. Absent limits preserve stock framing, UTF-8/EOF handling and vendor error excerpts. Logical budgets do not promise a bound on allocator, TLS or reqwest memory overhead.

## Function resolution

- Each adapter's `stream_chat` captures its config's `response_limits()` and calls the private `response::decode`. This module owns reqwest I/O; ports retain no HTTP dependency.
- `ResponseFrames::new(None)` delegates to the existing `SseFrames`. Its limited reader uses `ResponseLimits::add` before buffer extension, validates complete UTF-8 lines, bounds data payload bytes/counts before owning strings and passes bounded strings to the vendor's `observe_line` JSON parser.
- DeepSeek and OpenAI Chat use their wire accumulators. OpenAI's `Decoder` delegates limits/lifecycle to either Chat or the public Responses accumulator. Checked indexes precede resizing; combined id/name/prospective arguments precede cloning/appending.
- Anthropic `check_replay_frame` bounds retained block indexes/counts and prospective block/delta content before replay extension. `close` checks the full signed `AssistantReplay` envelope with allocation-free `content::serialized_size` before emission.
- `response::finish` is the adapter's local termination helper; `decoder.finish()` is the framing tail method. Bounded Chat/DeepSeek require `[DONE]`, Messages requires `message_stop`, and Responses requires `response.completed`/`response.incomplete`. Provider/parse failures close the accumulator directly.
- `error_body` checks streamed bytes before retaining/decoding them. Anthropic/OpenAI retain `error_body_snippet`; DeepSeek retains its original byte-based `truncate`. Setting the body stream to `None` releases ownership before terminal delivery; dropping an awaiting outer stream also drops the body.

## Execution traces

- 1024 accepted partial line bytes → one extra byte → limit refusal before extension → pending bytes cleared → one error → body dropped → no finished/call/replay batch. A larger chunk of many short lines uses per-line budgets and a separate whole-response budget.
- Individually accepted tool fragments → prospective combined call content exceeds event budget → retained calls and queued events cleared → terminal error. Repeated indexes in one frame see the already-updated slot. An excessive or malformed index cannot fold into slot zero.
- Responses done item omits id/name → those retained fields still contribute to prospective size → oversized replacement arguments refuse before copying.
- Individually small signed-thinking frames → escaped blocks plus replay metadata exceed the final envelope budget → error rather than `AssistantReplay`/`Finished`.
- HTTP 429 with 257 bytes under a 256-byte error budget → limit failure before owned extension, rather than reading all bytes and excerpting afterward. An exact-bound body retains its normal HTTP status diagnosis.
- EOF with a text/tool tail but no protocol terminator → bounded failure; stock EOF still closes its accumulated response. A legitimate terminator in an unterminated final line succeeds. Response termination/failure and quiet cancellation release a local HTTP socket while the caller still holds the outer event stream.

## Findings fixed

Removed duplicate getters and obsolete framer aliases/docs; qualified tool-name validation; fixed omitted Responses fields, malformed index folding, assembled argument/replay growth, missing final replay wrapper/escaping checks, partial named-call success, post-error payload handling and DeepSeek's changed stock error excerpt. Socket observers now wait off the runtime thread, letting Hyper process cancellation. Verification uses an isolated `NANUS_HOME`, so saved provider selections cannot turn the no-key fixture into a configured run. No user selection/keychain data is changed.

## Regression and edge evidence

Thirty-five new fixtures cover positive/rejected budgets, overflow, fragmented/invalid UTF-8, exact and one-over boundaries, aggregate bytes/data counts, sticky failure, malformed/huge indexes, retained-field and multi-frame assembly, signed replay envelopes, all four local HTTP grammars, EOF tails, malformed/incomplete streams, error excerpts, response release and quiet cancellation. Existing wire, runner, replay/persistence, stock and minimal-feature tests remain in the required gates.

**Budget semantics:** event bytes mean SSE JSON payload/assembled call content, with an additional full serialized signed-replay check. Event count means SSE data payloads, including unknown kinds and excluding `[DONE]`; it is not the number of `LlmEvent`s emitted from them. Hosts must separately limit decoded-event totals, retained records, deadlines and tool effects. Raw response totals include comments/framing and whole chunks already received, including bytes coalesced after a protocol terminator.

## Verification

All commands use Rust `+1.98.0`; clippy gates include `-- -D warnings`.

| Gate | Actual result |
| --- | --- |
| `cargo fmt --all --check` | exit 0 |
| `cargo clippy --workspace --all-targets --all-features` | exit 0, no warnings |
| `cargo nextest run --workspace --all-features` | 1,438 passed; 14 existing live tests skipped |
| `cargo test --workspace --doc` | 11 passed |
| TUI no-default clippy / tests | exit 0; 342 passed |
| Bundle no-default clippy / nextest / doctests | exit 0; 134 tests and 2 doctests passed |
| Locked standalone embedded example | 7 passed; 10 with `--features providers` |

Logs: `/private/tmp/nanus-response-bounds/`, with final results in `gates.json` and
`workspace.log`. The first targeted run failed its two blocking socket observers and the
machine-dependent no-key fixture. Further review found partial-call and stock DeepSeek excerpt
regressions: the partial-call fixture failed before its correction, while cross-scope source review
caught the excerpt change and a new byte/envelope fixture verifies its preservation. A later successful workspace run flagged one
pure fixture as leaky under Nextest's 100 ms pipe timeout; the fixture passed alone and the final
full run passed without a leak flag. Failed/intermediate logs are retained separately rather than
substituted for the final run. No live-provider requests were made.


## Verdict

**CORRECT for the tested opt-in local transport contract; high confidence.** Native Windows and live-provider acceptance remain untested. Hype's immutable dependency pin and provider factory have not adopted this unpublished API. Text-model ceilings and the rest of Hype's production migration remain incomplete. Exact-protocol selection was implemented afterward; see [its separate certificate](2026-10-02-exact_protocol.review.md).
