# Change: Bounded, recoverable context management

**Status:** Implemented locally, unpublished · **Date:** 2026-10-06 · **Owner:** Ant Stanley · **Target:** domain, ports, runner, adapters, store, link, interface, CLI

The normative proposal is the research specification
[`2026-10-05-context_management.md`](../../../clm-research/.specs/changes/2026-10-05-context_management.md)
(CM-01 … CM-13, acceptance cases T01–T35). This record tracks its implementation in this checkout.
The implemented contract is [`docs/context-management.md`](../../docs/context-management.md), with
its boundary schema [`docs/context-management.schema.json`](../../docs/context-management.schema.json).

## What was implemented

| Package | Where | State |
|---|---|---|
| W1 pure types, fragments, protected floor, compiler, fitting, proposals | `nanus-domain/src/context/managed/` | Implemented |
| W2 v3 records, prefix encoder, checkpoint port and store, recovery, frontier protocol and client | `nanus-domain/src/session.rs`, `nanus-ports/src/context.rs`, `nanus-adapter-store`, `nanus-link`, `nanus-tui` | Implemented |
| W3 context tools, staged transaction, provider capability and preparation, CLI | `nanus-bundle/src/agent_loop/managed*`, `context_tools.rs`, the three adapters' `managed.rs`, `nanus-cli` | Implemented |
| W4 recall, ranged file reads, shell archive sinks, quota, GC, deletion retirement | `nanus-bundle/src/recall.rs`, `nanus-adapter-local`, `nanus-adapter-store/src/store/` | Implemented |
| W5 manual policy, goal staleness, attempt accounting, benchmarks | `compile.rs`, `managed/settle.rs`, `nanus-bench/benches/managed.rs` | Implemented; evaluation not run |

The canonical pages named by the proposal's merge plan — `design.md`, `sessions.md`,
`architecture.md`, `docs/README.md`, `features.md`, `status.md`, `tui.md`, `benchmarks.md` —
carry its Add/Modify blocks, and the record-admission and Responses proposals carry its
coordination paragraphs.

## Acceptance evidence

Deterministic tests on macOS cover T01–T33 and T35 at the level each names; the names are listed
in `docs/context-management.md#what-is-and-is-not-verified` by area. The independent semi-formal
review and its outcome are recorded in
[`2026-10-05-context_management.review.md`](2026-10-05-context_management.review.md).

Open, by design or by authorisation:

- **T34** — the held-out exact-retention and coding-task evaluation needs paid, repeated live runs;
  none were authorised or run. Managed context therefore stays opt-in, and nothing claims it helps.
- **Live provider acceptance** of managed requests — fixtures only.
- **Power-loss durability** — not claimed; process-crash atomicity only.
- **Native Windows execution** — cross-target lint of the local and store adapters only.
- **Child-session handoff** — specified for a future feature; not implemented.

## Deviations from the proposal

- Context link frames use the `Frame` enum's existing `frame` tag rather than the schema's `type`;
  their envelope and payload fields are as specified. Ordinary progress frames carry frame ids only
  inside backlog segments; exactly-once delivery follows from the attachment barrier.
- An empty captured stream that reached end of file is `complete` with zero bytes, as the receipt
  rules allow, rather than `unavailable`.
- Hard fitting places probes by binary search over the number of oldest eligible fragments hidden,
  and verifies the chosen candidate exactly; the result is the oldest-first fit the proposal
  describes whenever cost falls as fragments are hidden.
- Managed Anthropic requests usually send history in its neutral form, because the per-request
  notice changes the encoded prefix that signed replay is bound to; correctness is unaffected,
  the earlier reasoning and prompt cache are not reused.
