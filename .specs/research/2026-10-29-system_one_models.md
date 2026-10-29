# System One decision-model research

**Status:** Draft · **Date:** 2026-10-29 · **Owner:** Ant Stanley · **Scope:** reviewer desk research

This is documentation research for the [System One permission reviewer](../changes/2026-10-29-system_one_permission_reviewer.md).
A *System One* model answers typed questions about a supplied state with probabilities rather
than generating text. No paid request, local model run or benchmark was performed for this note:
every figure below is a vendor's own documentation or a single third party, and is marked as such.
Recheck each contract before implementing; the category is weeks old and moving.

## The five installed providers

The owner's question: which of Nanus's providers offer a System One model, so a key already
stored for the conversation can be reused for review.

| Provider | First-party decision API | Endpoint | Model ids | Same key as chat? |
|---|---|---|---|---|
| OpenAI | Yes. Beta 2026-10-06, GA 2026-10-22. | `POST https://api.openai.com/v1/decisions` | `gpt-6-luna` only; no dated snapshots listed | Yes: an ordinary platform key, `OPENAI_API_KEY`. **API key only** — no documented ChatGPT-subscription or Codex OAuth route, and the Codex auth page says to use platform keys for general API calls. |
| Inception | Yes (Mercury Decide). | `POST https://api.inceptionlabs.ai/v1/decisions` | `mercury-decide` only; dated slugs exist only on OpenRouter | Yes by every sign (same base URL and `INCEPTION_API_KEY`); the docs do not say so in words. |
| Anthropic | No. Classification is prompting through `/v1/messages`, which returns no probabilities. | — | — | — |
| DeepSeek | No. Chat completions, the Anthropic-compatible API and Search/Fetch only (changelog to 2026-10-17). | — | — | — |
| z.ai / Zhipu | No. Rerank and content moderation exist; neither is a typed-question API. The Coding Plan covers coding tools and MCP only. | — | — | — |

Sources: developers.openai.com `guides/decisions`, `reference/resources/decisions`, `changelog`,
`models/gpt-6-luna`; learn.chatgpt.com `docs/auth`; docs.inceptionlabs.ai `capabilities/decisions.md`,
`get-started/models.md`, `resources/error-codes.md`; platform.claude.com `api/overview`;
api-docs.deepseek.com and `/updates`; docs.z.ai and docs.bigmodel.cn `llms.txt`.

## Two wire dialects

The field has converged on TypeSafe's shape, with one large exception: OpenAI's.

### The System One dialect

`POST …/v1/systemone` (or Inception's `…/v1/decisions`), request `{model, state, questions}`:
`questions` is a map keyed by caller-chosen names, each `{type, instructions?, criteria?}` with
`type` one of `noul` (yes/no), `choice` (`criteria` map, 1–255 options) or `score` (`criteria`
array, 1–10 levels). Response `{model, answers, usage}`: `answers` is keyed by the same names;
a `noul` answer is `{"type":"noul","noul":p}` with `p` = P(true); `choice` carries `choice`,
`probabilities`, `confidence`; `score` carries `score`, `legend`, `probabilities`, `confidence`.

| Implementation | Path | Differences from TypeSafe |
|---|---|---|
| TypeSafe Jev | `https://api.typesafe.ai/v1/systemone` | The reference (OpenAPI 0.2.0 at `api.typesafe.ai/openapi.json`). Adds `box` and `assets`. Versioned ids `jev-1.14.0`, `jev-1.13.0`; aliases `jev-latest`, `jev-preview` move. 429 and 529 with `retry-after`; 32 MiB body. |
| Inception Mercury Decide | `https://api.inceptionlabs.ai/v1/decisions` | Field-identical for `noul`/`choice`/`score` (its own OpenAPI); adds `extract` (in the docs, not the OpenAPI discriminator); images as `image_url` parts in `state`; `additionalProperties:false`; 20 MiB body; no `retry-after`. Unversioned id. |
| Laya (local, Apache-2.0) | `http://<host>:8000/v1/systemone`, plus `/v1/systemone/batch` | Adds `routing`, `x_jev_confidence`, `answer_confidence`, `action`, and `usage.{state_tokens, state_tokens_dropped, truncated, truncated_questions}`; `LAYA_JEV_STRICT=1` trims to the Jev contract. **`confidence` is normalised entropy, not Jev's formula.** **Over-long input is truncated, not refused.** No auth unless `LAYA_API_KEY` is set. |
| OpenRouter | `https://openrouter.ai/api/v1/systemone` proxy; `/api/alpha/decisions` | TypeSafe's shape plus `provider`, `session_id`, `trace`, `user`; response adds `id`, `provider`, `usage.cost`. About 22 decision models from 13 makers behind one key; Respan and ScaleDown take their own `state` shapes. |

### OpenAI's Decisions dialect

`POST https://api.openai.com/v1/decisions`, request `{model, input, questions, …}`:

- `input` replaces `state`: a string or a Responses-style item array; content parts `input_text`,
  `input_image`, `input_json` (`data`, user role only).
- `questions` is an **array** of `{name?, type, instructions}`; `noul` is called `predicate`;
  `choice` takes `choices: [{value, description?}]` (2–255); `score` takes
  `levels: [{label, description?}]` (2–10). 1–400 questions per request.
- Optional `prompt_cache_key`, `prompt_cache_options`, `prompt_cache_breakpoint`, `safety_identifier`.

Response: `answers` is an **array in question order**, each with `type` and `name`; a predicate
answer is `{type:"predicate", name, probability}`; choice and score `probabilities` are arrays of
objects; score has no `legend`; a per-question **`refusal`** answer type exists. `usage` adds
cached and reasoning token details. $0.10 per 1M uncached input tokens; output not billed; ZDR,
HIPAA and regional processing available to eligible customers. The only latency claim is "about
10x faster than Responses".

## Speed, cost, accuracy

| Option | Latency | Cost | Source and caveats |
|---|---|---|---|
| Jev 1.13/1.14 | p50 ≈ 0.24 s (JevBench API page); 0.65 s p50 from Germany (JevBench v1.0) | $0.042/Mtok, ≈ $0.03 per 1k decisions | JevBench is one independent operator; its scoring changes between versions. TypeSafe's cookbooks: 111 ms for 14 batched questions. |
| Mercury Decide | p50 ≈ 0.34 s (JevBench) | $0.02/Mtok promotional, $0.04 list | JevBench composite 72.2 vs Jev 1.13's 71.4. |
| OpenAI Decisions (`gpt-6-luna`) | "about 10x faster than Responses" | $0.10/Mtok | JevBench composite 62.4 for "OpenAI Decisions". |
| Laya | 39.5 ms (T4, one question); ≈ 125 ms Apple silicon p50; ≈ 450 ms 28-thread CPU (4 questions) | $0, local; ≈ 3.4 GB RAM with both checkpoints | Self-reported. Over-confident as shipped (ECE 0.466 → 0.081 after temperature fitting). Guardrail page: injection recall 0.72, FPR 0.21; document-shaped indirect injections all missed. |
| LLM-as-judge baseline | 0.8–1.9 s | $0.1–0.25 per 1k | JevBench v1.0: GPT-5.6 Luna 97.1% vs Jev 96.3% accuracy on 242 decisions. |

## Guidance that shapes the design

- **Ask atomic, literal yes/no questions and combine them in code.** TypeSafe's primitives page and
  OpenRouter's "auto-approve permission prompts" cookbook (`reversible` and `serves_task`, both
  ≥ 0.9, after a static risky list; any error or timeout leaves the ordinary prompt).
- **Thresholds are per model version.** TypeSafe: start at 0.7/0.3 for general use, re-tune per
  version, do not fit to a handful of cases; examples use > 0.85–0.9 for destructive actions.
- **Run-to-run noise of up to ±0.08** was observed on identical requests (TypeSafe consistency
  cookbooks; JevBench re-run changed 3 of 242 answers).
- **Vendor `confidence` fields are not comparable** (Laya's is entropy-based; Jev's is a
  max-probability normalisation; OpenAI's is undocumented beyond its presence).
- **No vendor claims robustness to injected content in `state`.** Jev 1.13's jaggedness page lists
  it as a known weakness, alongside a bias towards the first `choice` option.

## Assumptions and open questions

**Assumptions**

- Vendor documentation read on 2026-10-29 reflects the services as deployed.

**Decisions**

- _Scope._ **The five installed providers, plus TypeSafe, OpenRouter and Laya.** The owner named
  Jev and Laya, and OpenRouter is the only multi-vendor route that keeps TypeSafe's shape.

**Open questions**

- Does OpenAI accept a ChatGPT-subscription authorization at any Decisions route? Nothing found.
- Will Inception or OpenAI publish versioned decision-model ids?
- Is Laya's `typed-decisions` checkpoint the right local default for command review? Only an
  evaluation on labelled tool calls can say.
