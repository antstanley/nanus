# Nanus change specifications

**Status:** Draft · **Date:** 2026-10-01 · **Owner:** Ant Stanley · **Scope:** Repo-wide proposals

Current behavior and architecture remain documented in [the documentation index](../docs/README.md). This directory holds requested change proposals, not a second canonical description of the implemented code.

## Change specs

- [Optional read_video extension](changes/2026-10-01-read_video_extension.md) — Proposed; repo-maintained FFmpeg sampling with WebM/AV1 and all-model delivery, preserving the stock toolset.

- [Library embedding, argument-aware policy and multimodal tool results](changes/2026-09-30-library_embedding_and_multimodal_results.md) — Proposed; supports the embedded Hype Studio host without Claude plugin support.
- [Cross-repository semi-formal review](../../hype-studio/.specs/changes/2026-09-30-embed_nanus_rust_agent.review.md) — review of this dependency and the app migration.

- [read_video design review and verification](changes/2026-10-01-read_video_extension.review.md) — Independent semi-formal findings, fixes and successful recheck; records schema checks and repository gate results.

## Research

- [Video-input provider and model research](research/2026-10-01-video_provider_support.md) — Four supported harness providers; official documentation reviewed on 2026-10-01; no paid benchmark or video capability promotion.

## Assumptions and open questions

**Assumptions**

- `docs/` and tested Rust contracts remain the authority for what Nanus implements today.

**Decisions**

- _Proposal location._ **`.specs/changes/`.** The owner requested a change spec for missing integration functionality on 30 September 2026.

**Open questions**

- None for the index; proposal acceptance questions live in the linked change spec.
