# Nanus change specifications

**Status:** Draft · **Date:** 2026-10-05 · **Owner:** Ant Stanley · **Scope:** Repo-wide proposals

Current behavior and architecture remain documented in [the documentation index](../docs/README.md). This directory holds requested change proposals, not a second canonical description of the implemented code.

## Change specs

- [Stateless Responses replay and function policy](changes/2026-10-04-responses_replay_and_schema_policy.md) — Local sealed adapter implementation; host instruction revisions, prospective cost measurement and genuine multimodal request input remain required before Hype readiness. Includes the scoped host integration certificate.

- [Optional admission before user and model records](changes/2026-10-04-model_record_admission.md) — Proposed in this checkout; isolated local candidate implemented and reviewed with final scoped tests/lint/Windows cross-compilation passing. Unpublished; no Hype consumer/adoption or live/native acceptance.

- [Optional complete tool-batch admission](changes/2026-10-03-tool_batch_admission.md) — Implemented locally, unpublished; complete projections, raw/normalized validation and held-selection callbacks pass scoped credential-free gates. Consumer budgets/authority and native acceptance remain separate.
- [Enforce declared image-envelope bytes](changes/2026-10-03-enforce_image_envelope_bytes.md) — Implemented locally, unpublished; exact success/failure file-byte enforcement passes credential-free library/downstream gates, with an inline semi-formal certificate; stock credential-aware gates and native Windows execution remain unrun.
- [Optional read_video extension](changes/2026-10-01-read_video_extension.md) — Partially implemented;
  repo-maintained FFmpeg sampling with WebM/AV1, preserving the stock toolset. Local caller-owned
  snapshots are implemented; remaining provider routes/native certification stay open.
- [Caller-owned video snapshot certificate](changes/2026-10-03-video_snapshot.review.md) —
  external source construction and opaque lifetime retention, with scoped regressions and
  no claim of Hype adoption or native Windows execution.

- [Library embedding, argument-aware policy and multimodal tool results](changes/2026-09-30-library_embedding_and_multimodal_results.md) — Proposed; supports the embedded Hype Studio host without Claude plugin support.
- [Cross-repository semi-formal review](../../hype-studio/.specs/changes/2026-09-30-embed_nanus_rust_agent.review.md) — review of this dependency and the app migration.

- [read_video design review and verification](changes/2026-10-01-read_video_extension.review.md) — Independent semi-formal findings, fixes and successful recheck; records schema checks and repository gate results.

## Research

- [Exact z.ai API admission review](changes/2026-10-03-zai_api_admission.review.md) — local
  model/endpoint effort, text/output and function-tool contracts; captured fixtures do not
  establish live provider acceptance or qualify Coding Plan/images.

- [Video-input provider and model research](research/2026-10-01-video_provider_support.md) — Four supported harness providers; official documentation reviewed on 2026-10-01; no paid benchmark or video capability promotion.

## Assumptions and open questions

**Assumptions**

- `docs/` and tested Rust contracts remain the authority for what Nanus implements today.

**Decisions**

- _Proposal location._ **`.specs/changes/`.** The owner requested a change spec for missing integration functionality on 30 September 2026.

**Open questions**

- None for the index; proposal acceptance questions live in the linked change spec.
