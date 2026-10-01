# Review: Optional read_video extension

**Status:** Draft · **Date:** 2026-10-01 · **Owner:** Ant Stanley · **Scope:** Initial design consistency and verification

Reviewed the [proposal](2026-10-01-read_video_extension.md) using spec-reviewer R1 inline.
A clean agent subsequently applied reasoning-semiformally and found two design gaps.
The owner requested both fixes; the same independent reviewer verified the revised contracts
with a targeted semi-formal recheck and found no remaining material findings.
This checks the proposed design against existing documentation and code boundaries;
it does not qualify a decoder/backend or assert implementation. The
[provider note](../research/2026-10-01-video_provider_support.md) is desk research.

## 1. Premises

P1: Targets Architecture → What each crate owns; Design → Seven tools, and the count is the design; Features → The toolset; Sessions → What a session is; Safety → Defaults, and what they do not protect you from; Testing → The tests that matter most.
P2: Add a repo-maintained optional FFmpeg tool with WebM/AV1 sampling and direct-frame or same-provider analysis delivery for every supported configured model.
P3: Preserve stock seven/five tools, inward dependencies, fail-closed access, exact capability evidence, version-1/2 reading and save-before-acknowledgement.

## 2. Reference resolution

All six affected-page entries have exactly one matching Add block. No unlisted canonical
target occurs in Proposed changes. Relative files and heading anchors were checked locally.

| Target (Add) | 1. File exists | 2. Heading exists | 3. Modify/Remove base | 4. Equivalent video contract already present | Result |
|---|---|---|---|---|---|
| Architecture → What each crate owns | Yes | Yes | Not applicable | No | RESOLVED |
| Design → Seven tools, and the count is the design | Yes | Yes | Not applicable | No | RESOLVED |
| Features → The toolset | Yes | Yes | Not applicable | No | RESOLVED |
| Sessions → What a session is | Yes | Yes | Not applicable | No | RESOLVED |
| Safety → Defaults, and what they do not protect you from | Yes | Yes | Not applicable | No | RESOLVED |
| Testing → The tests that matter most | Yes | Yes | Not applicable | No | RESOLVED |

## 3. Consistency trace

| Resolved block | Canonical now | Change proposes | Finding |
|---|---|---|---|
| Architecture (Add) | Minimal bundle with caller-owned handles/executors; kernel services revert; executors have no session projection. | Repo-owned optional crate and fixed installation, plus a proposed generic opt-in ToolAdmission hook/host ledger. | Existing registration fits; aggregate admission is explicit new infrastructure. |
| Design (Add) | Seven stock tools and five separate goal tools. | Explicit host extension leaves stock construction/assertions intact. | CONSISTENT. |
| Features (Add) | Generic registration and bounded image reads. | FFmpeg/FFprobe sampling with WebM/AV1 and auto delivery across the current model catalogue. | CONSISTENT. |
| Sessions (Add) | Version 2 text/image records, version 1 reading, atomic bounded saves. | Persist manifest/pixels or analysis answer; no raw video/remote handle and no version bump. | CONSISTENT. |
| Safety (Add) | Program access needs approval; host policy adds exact-call decisions; secrets scoped by account. | Execute access, declared same-provider analysis model/budget and host teardown. | CONSISTENT. |
| Testing (Add) | Byte-level captured requests, replay and live image promotion evidence. | Required codec fixtures, all-model routing, image-profile and decoder lifecycle tests. | CONSISTENT. |

INTERNAL: Revised after independent findings. All routes use the same FFmpeg-sampled JPEGs; native movie upload and
audio inclusion are outside this contract. Auto resolves direct frames only for a verified
main image profile, otherwise sampled-frame analysis. Unknown main vision does not deny auto;
Unknown analysis capability does. Every configured main-model/provider/plan route is an
acceptance target, with no main-model or credential-account switch. Analysis retains frame
metadata but emits text only; frame mode pairs metadata with returned images. All timestamps
are source-relative. Same-provider Flash profiles still need qualification. WebM demuxing
and AV1 decoding are separate mandatory requirements verified against certified builds.
Existing historical-image refusal is preserved when resuming against an incompatible model;
explicit analyze provides text-only video records when that portability is needed. Generation
budget policy now distinguishes endpoint-enforced ceilings from subscription full-ceiling
reservation and bounded local collection. Aggregate admission includes retained history,
all pending tool results, failure slots and selection epochs before dispatch.

## 4. Schema check

- $ref resolution: all references resolve to definitions in the inline fragment.
- Modified entity base: not applicable; four new extension-only entities, no core rewrite.
- Prose/schema parity: VideoReadArguments, VideoFrame, VideoReadManifest and
  VideoAnalysisProvenance are all described; no orphan definitions.
- JSON fragments parse. Request mode is auto/frames/analyze, default auto; max_frames and
  optional question apply to every route. Manifest mode is the resolved frames/analyze
  result; requested_mode records the original auto choice. All results carry sampled frame
  metadata, codec/container/stream and sampler/build identity; method/audio are constant
  sampled_frames/omitted. Backend provenance is required only for analyze.
- Full JSON Schema meta-validation was not run: the local Python environment lacks
  jsonschema. Runtime semantic rules are explicitly listed separately from structural rules.

## 5. Open issues and evidence limits

- Concrete worker containment, resource enforcement and binary distribution are acceptance
  dependencies, not implemented promises; resolve with the first host before shipping.
- DeepSeek Flash and z.ai Flash need exact image profiles and endpoint-specific live
  evidence before the required full-model routing claim can be implemented. Native-video
  contracts are research only and cannot block or establish this sampled-image tool.
- Current canonical docs contain unrelated stale prose (for example Features still says
  Responses has no image encoder, whereas current code and vision-evidence show live
  Responses image support). The design follows inspected code/evidence and records the
  discrepancy without editing unrelated canonical behavior.

## 6. Verdict

VERDICT: LIKELY_CORRECT
CONFIDENCE: medium-high
SUMMARY: Targeted independent recheck resolved both original findings at the design level and found no remaining material findings or new contradictions; implementation evidence remains required.
SUGGESTIONS:
- Resolve the documented worker/profile questions before accepting an implementation plan.
- Reconcile stale canonical image/protocol prose in a separately scoped documentation change.
- Run schema meta-validation alongside executable parsers when the optional package is built.

## Design revisions, 2026-10-01

The owner narrowed provider scope to the four already supported providers, then required
repo maintenance, FFmpeg sampling, standard available codecs plus WebM/AV1, and compatibility
with every supported configured model. The proposal now assigns crates/nanus-tool-video,
uses FFprobe/FFmpeg on every route, and defaults to auto output with same-provider image
analysis for main models without verified vision. New mandatory codec/build and all-model
routing gates replace the earlier eight-model/native-video scope. Ownership is decided;
backend image evidence and certified binary distribution remain implementation questions.

Spec/research/index/review were aligned. Document links, headings, schema references and
JSON examples were rechecked. Rust code was unchanged; repository gate evidence below is
from the initial drafting pass. No live provider calls or FFmpeg experiments were run for
these design revisions, and image profiles were not promoted by editing the spec.

## Independent semi-formal findings and fixes, 2026-10-01

The reviewer received no conversation history and reached its findings before reading this
review. Its certificate resolved real tool, runner, budget-estimator and subscription wire
contracts. Verdict was CONCERNS: P1 high confidence; P2 medium-high confidence.

- **P1: subscription output bound.** The original uniform 2048-token promise conflicted
  with Responses encoding, which deliberately omits max_output_tokens. A local reservation
  of 2048 could under-reserve generation and reasoning. The revision makes 2048 an answer
  target, uses endpoint-enforced limits where supported and requires a qualified full backend
  ceiling reservation on subscription. Local byte/deadline cancellation returns failure;
  the host retains or conservatively settles incomplete charges independently of manifests.
- **P2: aggregate frame admission.** ToolExecutor and ToolPolicy lack the transcript and
  pending batch; per-image validation followed by next-request fitting cannot refuse before
  decoding. Three four-frame calls could retain twelve images. The revision proposes generic
  ToolAdmission contracts in ports and an optional runner hook, with immutable projections,
  all-tool envelopes, atomic call-ordered reservations, failure slots, validation/commit,
  cancellation cleanup and selection epochs. Unknown producer bounds disable direct-frame
  installation. This infrastructure is a proposed implementation requirement, not existing.

Both counterexamples are explicit acceptance tests. The image-count fixture now uses an
enforced 128 KiB JPEG envelope to isolate eight-image capacity; default 512 KiB envelopes can
legitimately fail the encoded-byte cap earlier. No Rust implementation or capability
promotion is claimed by these design fixes.

Targeted recheck certificate:

- **Premises:** retain optional installation, all-model routing, stock seven/five tools,
  approval, inward dependencies and existing session content.
- **Contract resolution:** ToolAdmission is explicit proposed ports/runner infrastructure,
  not assumed executor/policy state. Subscription estimation is accounting-only, not a wire
  generation cap. Media and analysis interfaces remain extension-local.
- **Execution traces:** generation beyond 2048 stays covered by the full qualified reservation;
  incomplete charges cannot settle as zero. With other capacities sufficient, a third
  four-frame call is refused before source I/O. Oversized actual results become reserved
  bounded failures before success reporting/logging.
- **Regression checks:** admission grants capacity rather than permission; absent hooks
  preserve stock behavior; goal dispatch stays separate. No video-specific runner dispatch,
  content variant, session-version change or credential reuse is introduced.
- **Edge cases:** failure-only projection exhaustion stops before effects; mixed image tools
  and pending chunks share capacity; cancellation retains failure allowances until append;
  stale epochs deny dispatch and resume reconstructs durable capacity.
- **Conclusion:** both original findings resolved, no remaining material findings or new
  contradictions. LIKELY_CORRECT, medium-high confidence; generic admission implementation,
  exact endpoint ceiling/accounting, containment and image-profile evidence remain gates.

The recheck was read-only and did not run Rust gates, FFmpeg or paid provider requests.

## Repository verification

Only Markdown files changed. All AGENTS.md gates were executed; no ignored/live provider
test was enabled and no new Rust test was written for this design-only change.

| Check | Result |
|---|---|
| cargo fmt --all --check | Passed. |
| Workspace all-target/all-feature clippy | Passed, no warnings. |
| Workspace all-feature nextest, ci profile | Hit the documented no-credential composition test on this credentialed machine. |
| Workspace nextest excluding that exact test | 1385 passed; 14 ignored live tests plus the one exclusion skipped. |
| Workspace doctests | Passed. |
| TUI no-default-feature clippy and tests | Passed. |
| Bundle no-default-feature clippy, nextest and doctests | Passed. |
| Locked embedded fixture, with and without providers | Passed. |
| New Markdown links/anchors, closing blocks, JSON parsing and schema references | Passed. |

The first suite failure is environmental, documented in AGENTS.md and docs/testing.md;
no credential was removed or altered to make it pass. The full suite was rerun with
`-E 'not test(a_configuration_without_a_key_still_composes_unconfigured)'`.
Source implementations and lockfiles are unchanged. Original workspace remains clean.

## Assumptions and open questions

**Assumptions**

- Current Rust contracts plus vision-evidence.md resolve pre-existing canonical prose drift.

**Decisions**

- _Review scope._ **Consistency of an initial proposal.** Codec coverage and all-model
  routing require future implementation evidence, not a design-document check result.
- _Independent recheck._ **Both findings resolved in the proposal.** Preserve the original
  counterexamples as acceptance tests and distinguish proposed hooks from shipped APIs.
- _Environment exception._ **Exclude one documented credential-dependent test on rerun.**
  Preserves existing credentials while completing all remaining tests.

**Open questions**

- Worker/backend qualification remains as listed in the proposal; no drafting work is blocked.
