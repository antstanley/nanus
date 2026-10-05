# Exact z.ai API admission — semi-formal certificate

**Date:** 2026-10-03 · **Scope:** Local companion delta, not published or adopted by the downstream host

## Premises

P1. The host requires independent exact-model text/output and ordinary tool evidence at
the actual configured API endpoint; a provider name or multimodal advertisement is
insufficient. P2. Primary contracts distinguish API effort validation from Coding Plan
server mappings. P3. Existing OpenAI routing/Responses metadata, Coding Plan/gateway
behavior, explicit caller choices, unknown stock requests, minimal embedding and
credential ownership must remain. P4. Nanus acquires no skill/plugin/prompt composition.

## Primary contract evidence

- [GLM-5.3](https://docs.z.ai/guides/llm/glm-5.3),
  [GLM-5.2](https://docs.z.ai/guides/llm/glm-5.2), and
  [GLM-5.3-Flash/FlashX](https://docs.z.ai/guides/vlm/glm-5.3-flash) provide independent
  1M context/128K output and tool contracts. Decimal 1,000,000 context is an explicit
  conservative interpretation of the advertised size, not an asserted binary API limit.
- [Chat API](https://docs.z.ai/api-reference/llm/chat-completion) states output
  1..131,072, at most 128 function definitions, the function envelope and auto tool
  choice. The adapter does not emit a forced tool choice.
- [Thinking](https://docs.z.ai/guides/capabilities/thinking): GLM-5.3 API variants use
  enabled thinking with low/high/max. GLM-5.2 accepts none/minimal/low/medium/high/xhigh/
  max with enabled thinking, including server mappings and skipped reasoning.
  Coding Plan's additional GLM-5.3 mappings are not API admission evidence.
- Flash/FlashX advertise multimodal input but have no verified local image profile;
  images stay Unknown. Text-only GLM-5.3/5.2 report Unsupported. No paid/live call occurs.

## Function resolution

`OpenAiLlm::capabilities` dispatches Vendor::Zai to private `zai::capabilities`; the
existing OpenAI Responses branch remains unchanged. `known_api` checks Vendor,
literal official base URL with trailing slashes removed, `resolve_protocol` Chat
result, and each exact id. It never reads environment/credentials or contacts HTTP.
The caller's request model, not only the configured model, is checked.

`effort_levels` selects `zai::efforts` only for verified API requests; otherwise it
calls the unchanged Vendor table. `tool_call_support` resolves through the existing
module to `zai::tool_support`, using request override first, then captured config.
`estimate_request` invokes checked protocol, `zai::validate`, image and generic tool
validation, then the real Chat encoder and shared byte/token estimator. It leaves
fit available for runner elision. `stream_chat` retains known Unsupported tool/image
refusal and uses `requires_preflight` for known API requests even without caller
budgets; `validate_estimate` prevents HTTP before serialization/client dispatch.

`OpenAiConfig::new` and `with_base_url` choose the verified API default max only
after the constructor's actual endpoint is known. Existing setters retain captured
values. The stock bundle's `Selection::effort` keeps explicit config first, accepts
its new API plan default only for exact official endpoint/model combinations, and
otherwise uses its unchanged neutral fallback. Composition subsequently installs
that resolved effort, rather than undoing the adapter's valid default.

## Execution traces

Before: `glm-5.3` API + default Medium → literal enabled/medium wire rejected by the
provider; tools/text metadata was Unknown. After: exact API + absent effort → Max,
known text/output/tools; explicit Medium still wins and now refuses before TCP.
An explicit Low request overrides invalid configured Medium and remains admissible.
GLM-5.2 None/Minimal → enabled/none or enabled/minimal remains valid and unchanged.

Known API text + output 131,073/0 → local terminal error before TCP even without
tools or caller context budget. Serialized input + reservation = 1,000,000 fits;
one additional ASCII byte produces 1,000,001 and refuses before TCP while below the
record byte limit. 128 functions fits; 129 refuses. Grouped two-call replay retains
both ids and paired success/error results even with an empty tool-definition list.
Official hostname resolved to loopback + valid text → actual local TCP connection,
then intentional TLS failure; this positive control checks transport wiring without
sending an authenticated provider request.

Coding Plan/gateway/unknown model + absent effort → original Medium and Unknown
capability; existing permissive stock encoding/dispatch remains. Changing a Coding
endpoint to API retains explicit/captured Medium, and API admission refuses it.
Different Vendor, explicit port, HTTP URL, query, near-match model ids inherit no
API evidence. Trailing slash retains the same verified API identity.

## Findings resolved

F1. The stock composer overwrote the constructor default with neutral Medium; the
API plan and actual-endpoint/model-scoped selection default now agree. F2. An
unconditional API plan Max would change custom endpoint/unknown-model behavior;
the selection guard preserves those old defaults, with pure table fixtures. F3.
An initial bundle edit used an unimported ReasoningEffort name; full workspace
Clippy caught it, and qualified port names fix it without aliases/suppression.
F4. An initial over-context fixture added a whole message; the final fixture adds
exactly one serialized ASCII byte and independently asserts the exact summed bound.
F5. Existing generic Unknown-provider coverage named a now-verified exact z.ai
model; it now uses a genuinely unknown id, while exact z.ai behavior has independent
matrix/encoder/pre-HTTP fixtures. Initial and corrected logs are retained.

## Regression evidence and limitations

Final adapter nextest: 93 passed, 0 skipped, including seven new exact API fixtures.
Pure provider table nextest: 13 passed, 143 filtered; one new default/scope fixture.
Workspace all-target/all-feature and minimal/runtime-free warning-denying Clippy pass;
whole-workspace formatting passes.
Minimal bundle nextest passes 139 tests; runtime-free TUI nextest passes 342 tests.
The adapter doctest command passes with zero examples. Fixtures use fictional keys, local sockets
and fake/caller-owned ports; no OS credential store or personal Chrome profile runs.
Evidence and baseline/source hashes were kept locally, outside the repository.

Workspace stock composition tests and workspace doctests remain unrun because they
can invoke the real credential store; a temporary Nanus home does not isolate macOS
Keychain. New metadata reports documented ordinary tools, not a captured live model
follow-up or image promotion. The stock Coding Plan currently selects FlashX while
its model page says that variant is not yet available there; this pre-existing
stock-plan issue is outside the host's API-only seam and remains unresolved. Native
Windows execution, immutable publication/adoption, host read_video ports and original
live/platform/package/recovery gates remain separate. No branch is pushed.

## Verdict

**CORRECT** for the traced local API contracts/admission and preservation of unknown
stock paths; confidence high from endpoint/effort matrices, actual encoder/replay,
exact numeric boundaries, negative pre-TCP tests and the positive loopback control.
Complete migration and live/provider/platform acceptance remain incomplete.
