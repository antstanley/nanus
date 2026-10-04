# Change: Enforce declared image-envelope bytes

**Status:** Implemented locally; unpublished; verification scoped · **Date:** 2026-10-03 · **Owner:** Ant Stanley · **Target:** Generic tool-result admission

Enforce `ImageEnvelope.max_bytes_each` on every returned image, as well as its image
count. The declaration used to reserve request capacity must bound the actual
observation. Keep this a generic runner contract, with no desktop, skill, plugin,
provider-selection or installation concepts.

## Motivation

Published Nanus `80a0f79b5db9d5f3d10c8eedc467bd754fadb604` reserves
`max_images × base64_len(max_bytes_each)` before tool execution. Its
`AgentRunner::hold_to_envelope` checks the returned count but not each raw file's
byte length. Global image validation permits a larger valid image up to 512 KiB.
Such an observation can consume capacity reserved for another admitted call.

An isolated downstream fixture using that exact immutable revision returns a
valid PNG at its declared raw-byte limit and again with a declaration one byte
smaller. Both results are retained as successes. This proves the missing check;
it is not evidence that the proposed guarantee exists. The host's complete-record,
request/token and payment bounds remain separate responsibilities.

## Affected spec pages

| Canonical contract | Change |
|---|---|
| [Design → Only three fields of a tool can reach the model](../../docs/design.md#only-three-fields-of-a-tool-can-reach-the-model) | Describe local envelope enforcement; keep metadata off the provider wire. |
| [Safety → Defaults, and what they do not protect you from](../../SAFETY.md#defaults-and-what-they-do-not-protect-you-from) | Distinguish declared result-byte enforcement from prospective admission and source/worker authority. |
| [Optional video proposal](2026-10-01-read_video_extension.md#implementation-status-2026-10-02) | Record the resolved generic byte gap only after implementation and verification. |

Rust contracts remain canonical. No JSON Schema, session format or provider-wire
field is added; this is enforcement of an existing in-memory declaration.

## Contract changes

### Domain → `ImageEnvelope` documentation (Modify)

> `max_images` bounds the returned image count; `max_bytes_each` bounds the decoded
> image-file bytes of every returned image. The latter counts the encoded file's
> decoded bytes, not its decoded pixel buffer. Admission reserves their base64
> contribution before dispatch. Returned success and failure content must satisfy
> both fields before it is retained, reported or submitted to another request.
> A violated declaration becomes a bounded model-visible failure without images.
> An undeclared tool retains the existing global content/profile validation.

### Domain → Exact file-byte counter (Add)

> `nanus_domain::content::image_file_bytes(data: &str) -> Result<usize, ContentError>`
> shares the bounded canonical base64 decoder used by `validate_image`. It counts
> decoded file bytes without I/O or pixel decoding. It does not validate media magic,
> dimensions or pixels; the existing subsequent media/profile checks still do so.
> The global encoded-input ceiling bounds temporary decoding; accepted decoded files
> must satisfy the existing 512 KiB ceiling before their length is returned.

### Bundle → `AgentRunner::finish_result` / `hold_to_envelope` (Modify)

> Check actual raw image-file lengths against the declaration before emitting
> `tool_finished` or retaining a tool result. Preserve the complete call identity.
> Malformed base64, media or a violated envelope produces a bounded failure
> without partial image blocks. Never truncate, resize, re-encode or silently drop
> an image to satisfy a declaration. Preserve normal content/profile/request checks.
>
> Obtain an exact length only from successfully validated canonical base64. Check
> arithmetic and padding; do not use the encoded length alone, which cannot
> distinguish consecutive raw lengths sharing the same base64 quantum. The global
> image bound limits validation allocation regardless of the declared byte value.
> Accept equality and refuse the first raw byte above the declaration.
>
> Apply this to image-bearing success and failure outcomes, including a zero-byte
> declaration. Text-only outcomes remain unaffected. No image is permitted when
> the declared count is zero. A declaration is capacity metadata, not permission,
> and it cannot bypass `ToolPolicy`, cancellation or source validation.

### Design and safety → Embedding boundary (Add)

> Per-result envelope enforcement preserves the byte promise made during image
> admission. It does not establish that the full next provider request, token
> projection, host audit/checkpoint record or separately paid analysis will fit.
> Embedding hosts retain their own smaller bounds and authority/worker lifetimes.
> Only tool name, description and parameters reach the provider; image envelopes
> remain local metadata. No new tool, skill or Claude plugin is introduced.

## Implementation notes

1. Extend `crates/nanus-bundle/src/agent_loop.rs`'s `hold_to_envelope`, reached
   through `finish_result` for every registered outcome. Reuse
   `nanus-domain/src/content.rs` validation and its bounded base64 contract.
2. Add generic runner cases in `crates/nanus-bundle/tests/admission.rs`: exact
   raw byte limit, one byte over within the same encoding quantum, all three
   base64 padding cases, invalid base64/media, zero-byte/count declarations,
   and image-bearing failures. Preserve the original call id and bounded refusal.
3. Exercise a mixed admitted batch: an executor returning a larger-than-promised
   image cannot install pixels into another call's reserved capacity. Keep
   existing before-execution byte/count refusal and current-turn-history tests.
4. Keep minimal/default-disabled bundle and downstream examples clean. Run
   formatting, workspace all-target/all-feature Clippy, credential-free domain and
   minimal-bundle nextest/doctests, plus standalone downstream gates. Explicitly
   leave credential-aware stock workspace tests/doctests unrun: changing the home
   directory does not isolate the real Keychain. No live provider or secret is
   needed to prove this generic result contract.

## Acceptance criteria

- A valid image at its declared raw-byte limit is retained; the next byte becomes
  a bounded failure without images, before progress completion or session replay.
- Exact byte enforcement applies independently to every image and outcome status.
- Undeclared tools still use their existing global/profile bounds; text-only
  outcomes, wire allowlists, stock seven/five counts and policy denial are preserved.
- No new HTTP request, source read, cleanup authority, metadata wire field or
  session-version change is introduced by validation.

## Semi-formal proposal review

**Premises.** P1: admission uses the existing declared per-image bytes. P2: actual
outcomes must honor that promise. P3: library validation must not acquire host
permissions, modify pixels or claim full-request admission.

**Resolution.** The public factory stores `ImageEnvelope`; `admit_images` uses
its byte value. `finish_result` reaches `hold_to_envelope`, then existing content
and profile validation. The published hold checks only count. A downstream fixture
against immutable 80a0f79b confirms the one-byte overflow remains a success. The
local hold checks count and canonical decoded file bytes before the existing
complete-media/profile validator; every image in success or failure content passes
through the same path.

**Trace.** Declared N, actual N: retain after validation. Declared N, actual N+1:
replace with an image-free failure before progress/log delivery. Denied or
prospectively over-capacity calls still never execute. Existing tool effects may
already have happened when an invalid returned outcome is refused; this change
neither rolls them back nor establishes physical worker completion.

**Fixed proposal findings.** State raw-file rather than pixel-buffer bytes; require
exact padding-aware size rather than encoded-length comparison; cover failure
content; preserve global bounds and distinguish full-request/token admission.

**Verdict.** LIKELY_CORRECT for the locally implemented result contract. A bounded
pure decoder rejects empty, malformed and globally oversized base64 before file-byte
comparison. Equality is inclusive; a one-byte excess fails even in the same base64
quantum. Existing validation still rejects malformed media and mismatched MIME.
The mixed-batch fixture refuses an oversized second image in the first call while
retaining the valid next call's pixels and identities. No new envelope is activated
on undeclared tools. This does not prove whole-request/token admission, host audit
ordering or physical effect rollback. The immutable consumer fixture proves the
published gap only; Hype's production pin remains unchanged by this local fix.

## Local implementation and verification — 2026-10-03

`hold_to_envelope` checks the new pure counter for every declared returned image.
The shared decoder also feeds the existing `validate_image`, preserving its media,
dimension and allocation checks. `ImageEnvelope` documentation now states raw-file
bytes and both outcome statuses. No wire fields, session versions or dependencies
change. Four new runner fixtures and one domain boundary fixture cover all three
padding cases, exact/next-byte limits, later images, malformed base64/media/MIME,
zero declarations, failure pixels, text-only results and neighboring batch calls.

Final domain/minimal-bundle nextest passes 320 tests with no skips; the all-feature
actual-runner admission suite passes nine cases with no skips. Workspace
all-target/all-feature warning-denying Clippy, minimal-bundle Clippy, formatting,
three domain/minimal-bundle doctests, standalone embedded default (7 tests) and
providers (10 tests), and Windows-target domain/minimal-bundle library Clippy pass.
Logs and source evidence are retained under `/private/tmp/hype-image-envelope`.
Credential-aware stock workspace tests/doctests and native Windows execution remain
unrun. No secret, personal browser profile or paid request is used. The fix remains
unpublished; original video/provider/platform acceptance is not closed by it.

## Merge plan

After implementation and the stated gates, update the Rust contract and affected
documentation with verified behavior. Mark this proposal Implemented, then fold
its documentation blocks into the canonical pages, move it to `changes/merged/`
and update the index. Do not mark the broader video proposal complete from this fix.

## Assumptions and open questions

**Assumptions**

- The caller owns tool effects and any stricter whole-result/request budgets.
- Existing global validation bounds image files and rejects malformed media.

**Decisions**

- _Existing declaration._ **Keep the existing envelope/admission API.** Add the
  pure exact file-byte counter shared with media validation; no callback, wire
  field or new admission framework is needed for this specific guarantee.
- _Refusal._ **Keep the call identity and return no pixels.** Partial evidence
  would misrepresent the executor's observation and the reservation it exceeded.

**Open questions**

- None for this narrow contract. Generic complete-request/token projections and
  remaining video/provider/platform acceptance stay in their existing proposals.
