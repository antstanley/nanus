# Exact protocol and capability review — 2026-10-02

**Scope:** the exact-protocol prerequisite in the [embedding proposal](2026-09-30-library_embedding_and_multimodal_results.md). This is unpublished local work; no Hype dependency-pin update, Chat image promotion, live acceptance or production Nanus integration is claimed.

## Premises

P1. `set_protocol(ChatCompletions)` historically sets a fallback; model-dependent `protocol_for` overrides it for newer OpenAI IDs. A host must be able to choose an exact wire without silent redirection.

P2. Capability evidence belongs to an exact model, vendor, endpoint and wire. Recorded Responses support cannot establish Chat or proxy image support. Unknown combinations refuse images and explicit capability-dependent estimates before HTTP.

P3. Existing Automatic routing, stock plan hints and output encoding remain. New exact policies may reject unsupported combinations; the public API's output ceiling cannot disappear merely because its fallback is Responses.

## Function resolution

- `ProtocolPreference::Automatic` defaults in `OpenAiConfig::new`. `set_protocol_preference` checks the vendor/endpoint before committing; `protocol_for` returns Exact first, otherwise the unchanged model/fallback routing. The historical `set_protocol` remains a fallback setter.
- `resolve_protocol` rechecks exact compatibility after endpoint edits. `OpenAiLlm::new` maps an incompatible policy to typed `OpenAiError::UnsupportedProtocol`; streaming/estimation use `checked_protocol`, whose port refusal is `LlmError::Unsupported`. Neither starts HTTP on refusal.
- `checked_protocol` also rejects an exact Responses request whose explicit output ceiling the subscription endpoint cannot honor. Responses encoding calls `sends_output_ceiling`: Automatic keeps the old plan hint; Exact uses the actual endpoint. Exact API requests therefore send their selected ceiling even if the fallback is Responses.
- `capabilities` requires the OpenAI vendor, one of the two exact evidence-bearing base URLs, selected Responses and an exact model profile. Chat, unknown endpoints, mismatched vendors and unprofiled IDs return default Unknown metadata. No endpoint alias, future model or provider-wide profile is inferred.
- `estimate_request` checks policy and image capabilities before raw encoding, then uses the existing full-wire estimator. `stream_chat` checks before request construction and selects its URL/decoder from the resolved wire. Raw `encode` remains documented wire inspection; it selects the requested shape but does not establish supported capabilities or accepted HTTP.

## Execution traces

- Newer model plus fallback Chat → Automatic still chooses Responses. Exact Chat plus that same model → Chat URL, message payload and Chat decoder. Changing fallback/model cannot redirect Exact. Exact Responses plus an older model → Responses URL/items/decoder.
- Exact Responses on z.ai, or Exact Chat on the known subscription endpoint → typed unsupported refusal → previous preference preserved. Changing an accepted Exact Chat configuration's endpoint to subscription → revalidation/construction refusal.
- Exact Chat plus a model promoted on Responses → Unknown image/ceiling metadata → image estimation and dispatch refuse before HTTP. A local custom endpoint likewise cannot inherit the real API's image evidence; its listening socket observes no connection.
- Exact Responses at the public API with a Responses fallback → explicit 8192 output ceiling still encoded/reserved. Exact Responses at subscription → no unsupported output field; an explicit requested ceiling refuses before estimation/HTTP rather than being silently ignored.

## Regression and edge evidence

Nine new behavioral fixtures cover Automatic/Exact selection and restoration, future/older model switches, unsupported policy nonmutation, endpoint-edit revalidation, the six promoted models on both evidence-bearing endpoints, unprofiled/mismatched/lookalike/HTTP/query/proxy endpoints, image refusal, output ceilings and actual local HTTP paths/payloads/decoders. Existing backend capability coverage now names the backend endpoint instead of labelling a loopback proxy as Supported. Existing encoder/store-reload fixtures remain raw wire proofs, not live image promotion.

The code review fixed two cross-scope findings before final gates: capability queries had ignored the actual wire/endpoint, and Responses output-field presence had followed a legacy plan hint instead of the exact selected endpoint. Checked estimation also rejects images before copying their wire encoding. No failed regression test result is invented for these source-review findings.

## Verification

All eleven required `+1.98.0` gates passed; clippy ran with `-- -D warnings`:

- Workspace fmt/clippy, 1,447 nextest tests (14 existing live skips), 11 doctests.
- TUI without default features: clippy and 342 tests.
- Minimal bundle: clippy, 134 tests and 2 doctests.
- Locked standalone embedding example: 7 tests; 10 with explicit providers.

The full workspace run had no Nextest leak flag. Logs and gate exit codes are in
`/private/tmp/nanus-exact-protocol/`. `NANUS_HOME` was isolated; no live model request or Chrome
launch was made. Native Windows and changed-policy live acceptance were not run.

## Verdict

**CORRECT for exact routing and the tested capability/refusal contract; high confidence.** The two supported Responses endpoints retain their previously recorded profiles; this change adds no Chat image support or text-only model ceiling table. Hype still needs an adopted immutable revision, explicit factory policy and its production/release integration. Its current Chat contract is not silently changed.
