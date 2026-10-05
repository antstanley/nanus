# Change: Library embedding, argument-aware policy and multimodal tool results

**Status:** Proposed · **Date:** 2026-10-01 · **Owner:** Ant Stanley · **Target:** Nanus library crates

Make the existing Nanus runner usable inside a desktop Rust application with caller-owned adapters, per-call policy and wakeable cancellation, and preserve image tool results through persistence and provider requests. A downstream desktop host embedding the runner is the first consumer. This proposal changes library seams; it adds no Claude plugin loading, skill discovery, desktop UI, web tools or app-specific tools.

## Motivation

The embedding host is replacing its Claude SDK/Node agent sidecar with an embedded Rust library. Nanus already exposes its runner and ports, but `nanus-bundle` pulls concrete Unix adapters into every consumer, the default approval gate skips argument-aware checks for reads, cancellation can wait on an idle stream, and images are reduced to text before reaching a model.

Keep one agent loop and the existing CLI/TUI/service behavior. Add reusable library contracts at their actual boundaries so a caller supplies its own keychain, process supervision, prompts, tools and policy. A skill is ordinary trusted system-prompt text supplied to `AgentRunner::new`; it needs no new loader or plugin protocol.

## Affected spec pages

Nanus currently documents its canonical architecture and behavior in `docs/`, not a numbered `.specs/` set. These existing pages remain the authority; `.specs/` indexes proposals without inventing a parallel canonical spec.

| Existing authority                         | Headings / nature of change                                                                                                                                              |
| ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| [Architecture](../../docs/architecture.md) | What each crate owns: minimal library feature boundary                                                                                                                   |
| [Design decisions](../../docs/design.md)   | Only three fields of a tool can reach the model; Approval is a three-state axis, fail-closed at the default: host policy and preserved image blocks                      |
| [Features](../../docs/features.md)         | The agent; The toolset; What a request costs before the conversation; Model providers; Composition and the kernel: embedding, images, cancellation and capability limits |
| [Sessions](../../docs/sessions.md)         | What a session is; What a session says about itself; Resuming: versioned image content and legacy loading                                                                |
| [Status](../../docs/status.md)             | Known limits: distinguish embeddable Windows subset from Unix CLI/link                                                                                                   |
| [Testing](../../docs/testing.md)           | The tests that matter most: embedding and multimodal proof cases                                                                                                         |
| [Safety](../../SAFETY.md)                  | Defaults, and what they do not protect you from: optional host policy and cancellation ownership                                                                         |

## Proposed changes

### N1. Architecture → What each crate owns (Modify)

> `nanus-bundle` offers a minimal runner/tool library without concrete I/O adapters when `default-features=false`. Its default `stock-compose` feature retains the shipped `compose`, provider selection, authorization and remembered-selection modules and their concrete adapter dependencies. These modules and their public re-exports are feature-gated together; domain, ports, kernel, runner, tools, arguments and general runner errors remain available in the minimal build.
>
> The optional dependency list includes adapter-config, adapter-local, adapter-secret, adapter-store and the three provider-adapter packages currently assembled by `compose`. Callers can depend directly on the provider adapters they choose. Gate composition tests and helper modules that require those adapters, and audit `provider`, `authorize`, `selection`, `error` and `tests_support` imports rather than gating only `compose.rs`. The default CLI/service behavior is unchanged.
>
> The minimal subset compiles on Windows and macOS with caller-supplied `FsPort`, `ShellPort`, `ClockPort` and `LlmPort`. It does not link stock `LocalShell`, the secret fallback chain, a local socket or a TUI. This does not make the Unix CLI/service/link cross-platform. Nanus remains single-threaded with `Rc` handles/local futures; an embedding host constructs them on its own local executor and passes owned messages across threads. Any kernel mounting retains the existing async-prepare/sync-mount staging rule; direct runner construction does not mount the kernel.

### N2. Design decisions → Approval is a three-state axis, fail-closed at the default (Modify)

> An optional host `ToolPolicy` is consulted for every **registered** tool call before the default sandbox/approval gate and before execution, including `Read` calls that the default sandbox permits. It receives the actual immutable `ToolCall` (ID, name and parsed arguments) and the resolved definition's `ToolAccess`. Its local asynchronous result is `UseDefault`, `AllowOnce` or `Deny { reason }`. A policy error or cancelled decision denies; absence of a policy preserves today's default gate exactly.
>
> `UseDefault` follows the existing `SandboxMode`/`ApprovalPolicy`/`Approver` path. `AllowOnce` grants only this exact invocation, never a standing tool grant or a permission-mode change. `Deny` records an ordinary failed tool result paired with the original call ID and explains the refusal to the model. Unknown tools and malformed inputs still fail registry validation, regardless of a host grant. No input editing is implied by this contract. All tools retain truthful Read/Write/Execute classes.
>
> Policy futures do not hold a registry borrow. Decide each call in model order, then execute only allowed calls with the existing configured concurrency limit. A host can enforce tighter argument/path/network policy than the stock gate and request its own UI decision. Executor ports still recheck live filesystem scope immediately before effects; an approval is not an OS sandbox. Loop-internal goal tools remain internal session bookkeeping, with no new process/filesystem authority.

### N3. Features → The agent; Composition and the kernel (Modify)

> `AgentRunner::new` accepts an ordinary system-prompt string, validated configuration, registered tool handle and caller-owned model/clock handles. An embedding example builds a custom registry and runs a scripted turn on a current-thread runtime/local executor without config files, credential lookup, a service or any CLI/TUI dependency. The prompt can be a caller's skill/template text; Nanus adds no plugin namespace, manifest discovery or Claude plugin support.
>
> Add `run_turn_with_control` alongside the existing `run_turn`. It accepts a reusable caller-owned `TurnControl` with a local `cancelled()` future and `is_cancelled()` state. A cancelled control remains cancelled for the whole turn; callers create a new control for the next turn. The existing entry point retains its behavior, including `Progress::cancelled`; the controlled entry point observes both sources.
>
> Race cancellation against model-stream polling, host policy, default approval and tool awaits. A cancellation signalled before the first token or while a stream is silent wakes the turn promptly. Drop outstanding futures and record an Interrupted terminal reason exactly once. Poll control again immediately before dispatching an effect so an approval racing with cancellation cannot start a new action. Tool ports are required to make dropped work cancel-safe or provide caller-owned teardown; dropping a future alone does not promise that a child process died. The host owns joining workers, killing shell/renderer trees and cancelling its UI waiters. Nanus performs no automatic replay after interruption.

### N4. Design decisions → Only three fields of a tool can reach the model; Features → The toolset; Model providers; What a request costs before the conversation (Modify)

> Executable tool definitions remain unserialisable; tool schemas still send only name, description and parameters. Tool **results** preserve ordered typed text/image blocks in addition to a text summary for display. `render_content` becomes a display helper, never the sole representation stored for model replay. An image is inline bounded data plus a validated media type; file paths or remote URLs supplied by a tool are not automatically fetched by an encoder.
>
> `SessionEvent::ToolResult` and `Message::Tool` retain `content: String` as display/legacy text and add optional `content_blocks: Vec<ContentBlock>` as model content. Absence means legacy text only; a present empty list is rejected. New runner results carry the typed blocks. Session folding, prompt assembly and store round-trip retain their order and original call ID/error flag. Encoders use blocks when present and text when absent, so the same text is never sent twice.
>
> Add a defaulted `LlmPort::capabilities(model)` query returning `ModelCapabilities`, whose `image_input` is `ImageInputSupport` (`Supported`, `Unsupported`, `Unknown`) and whose optional image profile is defined below. The adapter instance’s configured protocol/vendor participates in the lookup, so a model name alone cannot select another endpoint’s profile. Input/context/output token ceilings are explicit optional metadata; an embedding host can refuse configurations with missing ceilings without changing the stock text-only entry point’s current behavior. This is a per-model/protocol capability, not a claim that every model from one provider supports images. Existing/fake adapters default to Unknown. Keep the seven-tool stock registry unchanged. Embedders may omit the read_image offer on unsupported selections; any offered call/result on Unsupported or Unknown is refused explicitly before it enters the provider request; never silently replace supported pixels with a placeholder. A provider/model switch revalidates any image-bearing history before issuing a request. No capability method reads credentials or performs network I/O.
>
> Implement only the initial exact model/protocol profiles below, with explicit failure before HTTP for image-bearing requests on other combinations. Anthropic nests ordered text/image content in a matching `tool_result` with the original `tool_use_id` and error flag. OpenAI Chat Completions emits all paired tool messages in model-call order before any image attachments. For an image-bearing result the tool message contains only an app-generated attachment marker naming that call ID; after the complete group of tool results, emit one user multimodal message per image-bearing call in call order, starting with the same call-ID label followed by its original text/image blocks in order. Use inline data URLs and the profile’s fixed `detail: high`; do not repeat its text summary as original content. Text-only results remain ordinary tool-role text. This avoids orphaning sibling calls and preserves result attribution on replay; it is an explicit encoder translation that must pass live acceptance before capability promotion.
>
> The separate OpenAI Responses encoder returns `unsupported-image-protocol` before HTTP for image-bearing requests in this change. Its existing subscription/OAuth text flow and the z.ai/DeepSeek text encoders retain their behavior. No arbitrary model ID, endpoint alias, provider label or stored session header enables an image profile automatically.
>
> Validate PNG/JPEG media type against decoded magic bytes; reject malformed base64, empty images, conflicting type, more than four image blocks per result, image file bytes over 512 KiB each, and tool-result serialized records over 4 MiB before retaining them. The embedding host imposes its own stricter 1 MiB record cap. Library limit failures are typed/model-visible failures, not panics. Memory validation checks encoded size before base64 decoding, reads bounded dimensions from verified PNG/JPEG data before pixel allocation, and rejects dimensions outside the enabled profile. No profile may allocate an unbounded decompression buffer. Transcript renderers show summaries and never print base64.
>
> Extend context estimation with the dimension-based profile charges below, plus text/system/tool-schema/framing costs and the actual configured output/reasoning reservation. Use checked integer arithmetic with upward rounding; overflow and missing profile/ceiling metadata are pre-HTTP failures. Estimate against the assembled request after encoder translation, counting replayed images every time and charging attachment labels once. Bound serialised model requests to 4 MiB and eight images, in addition to the existing result/session caps; count encoded bytes separately from image tokens. Keep whole-turn drop-oldest behavior, preserve call/result/attachment groups and report elision; if the current turn alone cannot fit, return a typed context-limit failure without HTTP. Estimates are safety reservations, not provider billing or a promise that an endpoint cannot reject a request. No price catalogue or dollar-cost estimator is added.

#### Embedding audit: exact protocol, text-model ceilings and pre-event bounds

A downstream integration audit of pinned Nanus revision `479f6de5fcab18899e5eb0e3268d4316ee8efe08` found three remaining generic seams. This evidence supplements the historical implementation section below; it does not claim the remaining work shipped.

- **Exact protocol selection.** `OpenAiConfig::set_protocol(ChatCompletions)` is overridden by `protocol_for` for current OpenAI model ids. Add a caller-selected exact protocol preference alongside the existing automatic routing. Automatic remains the stock default; Exact either uses that protocol or returns a typed unsupported error before HTTP. Never silently redirect an exact Chat Completions request to Responses. Capability/image-profile queries must use the actual configured vendor, endpoint and protocol, and tests must cover automatic routing, both exact preferences and protocol-mismatched images. The host's current proposal still requires Chat Completions; upstream Responses evidence does not satisfy that gate. Whether the host adopts Responses is an explicit consumer decision.
- **Text-model ceilings.** The pinned DeepSeek adapter inherits unknown capabilities and z.ai receives the OpenAI adapter's unknown metadata. Populate optional input/context/output ceilings for each offered API model from independently checked provider documentation, with exact-model entries and source/evidence references. No provider-wide guess, alias fallback or custom-model inheritance is permitted. Preserve Unsupported/Unknown image input for text-only combinations. Tests prove valid output reservations fit each entry, invalid ceilings refuse before HTTP, and custom ids stay unknown. Stock text behavior is preserved when no embedding ceiling requirement is selected; an embedding consumer may refuse incomplete metadata.
- **Bounds before events.** The pinned `SseFrames::push` extends an unbounded pending byte vector before producing events, so a consumer's event-byte limit cannot bound an unterminated SSE line. Add opt-in adapter transport/decoder limits selected by callers before streaming: partial line and decoded-event bytes, total response bytes, event count and tool-call slots. Check remaining capacity before extending buffers or allocating decoded values; a large chunk cannot be buffered first and checked later. Bound non-success response bodies before decoding rather than calling unbounded `Response::text`. On overflow return one ordinary typed/model error, close/drop the response, and preserve no partial assistant replay as a completed response. Fragmented UTF-8, exact-bound and one-over-bound lines/chunks, many small events, excessive indexed calls, oversized error bodies and quiet cancellation must have fixtures. The consumer still owns its request/turn deadlines and cancellation policy; no desktop/setup/prompt concepts enter Nanus.

**Local implementation, 2026-10-02:** `ResponseLimits`/`ResponseFrames` and the three API
config setters now implement the pre-event seam on the current working copy. Limits count raw
response bytes and SSE data payloads (including unknown kinds), not decoded `LlmEvent` count;
consumers retain their own decoded-event/record limits. JSON framing and call-content checks precede
copies/extensions, and complete signed replay checks its serialized envelope before emission.
Opt-in streams refuse malformed UTF-8/JSON and missing protocol termination, clear retained partial
calls/replay, emit one terminal error and drop the response. Absent limits preserve stock behavior.
Local real-socket and accumulator fixtures cover all four API grammars; verification is recorded in
[the response-limit review](2026-10-02-response_limits.review.md). This is unpublished local work,
not a new immutable dependency pin or native Windows/live-provider evidence. Text-model ceilings
remain unimplemented; exact-protocol progress is recorded below.

**Local exact-protocol implementation, 2026-10-02:** `ProtocolPreference::Automatic` preserves
stock routing; `Exact(ChatCompletions | Responses)` overrides model/fallback routing or returns a
typed unsupported refusal for an incompatible vendor/endpoint. Rejected preferences do not mutate
configuration, and endpoint edits are rechecked before construction/estimation/HTTP. Capability
queries now require the actual selected Responses wire, exact model and one of its recorded API or
subscription endpoints. Chat and custom endpoints remain Unknown; no Chat image profile is promoted.
Exact Responses output controls follow the actual endpoint, while Automatic retains stock plan
handling. An explicit ceiling that the exact endpoint cannot honor is refused before HTTP.
See [the exact-protocol review](2026-10-02-exact_protocol.review.md) for nine fixtures and all required
verification gates. This is unpublished local work and does not adopt a new downstream dependency pin or
change the host's proposed Chat contract; text-model ceiling metadata remains open.

#### Provider admission evidence and tool-support seam — 2026-10-03

A fresh public-source audit resolves main to [`80a0f79b5db9d5f3d10c8eedc467bd754fadb604`](https://github.com/antstanley/nanus/commit/80a0f79b5db9d5f3d10c8eedc467bd754fadb604).
That revision publishes the optional video tool and includes the Windows source changes;
it does not publish the local exact-protocol/response-limit changes above. The local z.ai text-ceiling
and generic tool-support requirements remain open; DeepSeek progress is recorded below. The earlier dated local/public assessments are
historical evidence, not statements that the video extension is still unpublished.

Checked primary contracts supply the following starting evidence, not completed capability
entries or image promotions:

| Exact API model | Context / maximum output evidence | Tool or image qualification |
|---|---|---|
| `deepseek-flash` | 1,048,576 / 393,216 tokens in the [models response](https://api-docs.deepseek.com/api/list-models/) | The current response lists text and image input; preserve Unknown images until this exact wire/replay/profile gate passes. |
| `deepseek-v4-pro` | 1,048,576 / 393,216 tokens in the same exact-model response | The response lists text input only; do not inherit Flash's image modality. |
| `glm-5.3` | 1M context / 128K maximum output in the [model contract](https://docs.z.ai/guides/llm/glm-5.3) | Text model; do not infer the limits of `glm-5.3-flashx`, `glm-5.3-flash` or `glm-5.2` from this entry. |
| `gpt-6-astra` | 1,050,000 / 128,000 tokens in the [model contract](https://developers.openai.com/api/docs/models/gpt-6-astra) | Tools require Responses; Chat accepts text without tool calling. Image evidence stays independent. |
| `gpt-6.1-sol` | 1,050,000 / 128,000 tokens in the [model contract](https://developers.openai.com/api/docs/models/gpt-6.1-sol) | Tool calling requires Responses; Chat supports text completion without tool calling. |
| `gpt-6-luna` | 1,050,000 / 128,000 tokens in the [model contract](https://developers.openai.com/api/docs/models/gpt-6-luna) | Chat tool calling requires explicit `reasoning_effort=none`; the configured/default reasoning effort is a different case. |

Retrieve and retain each exact model/endpoint contract when implementing its entry. Context
and output evidence does not establish a separate input limit: supply a justified input bound
and always subtract the actual output/separate-reasoning reservation from combined context.
Do not convert ambiguous `M`/`K` prose to a precise integer without supporting evidence, inherit
limits across aliases/models, copy pricing quotas into context metadata, or promote image
support because text metadata is now known. Custom endpoints/ids stay unknown.

**Local DeepSeek text metadata, 2026-10-03:** the working copy now supplies exact
`deepseek-flash` and `deepseek-v4-pro` entries only on `https://api.deepseek.com` (with
an optional trailing slash). Both report 1,048,576 combined-context tokens and 393,216
maximum output tokens, independently listed in the linked models response. The input
bound is the same combined-context upper bound, not a separately documented input quota;
actual output and separately reserved reasoning are still charged against that context.
Custom hosts, paths, aliases and ids inherit no limits. Flash images remain Unknown with
no profile; Pro images are Unsupported because its exact contract lists text only.

The concrete estimator and stream reject zero or above-maximum output before HTTP,
including configured defaults and requests without an explicit context budget. Valid explicit
output takes precedence over the retained 256,000-token library default; that default is
neither the server default nor its maximum. Existing neutral effort mapping is unchanged.
Seven new fixtures cover the actual encoders, zero/one/maximum/one-over output, assembled
input-plus-reservation below/at/above context, endpoint/id isolation and no-contact refusal.
A positive TCP control proves the fixture routes valid requests to its own local listener.
No provider request or real credential is used.

Verification: 48 DeepSeek tests pass; warning-denying all-target/all-feature workspace
Clippy and rustfmt pass. Minimal embedding passes 139 tests and two doctests; standalone
consumers pass seven tests without providers and ten with explicit synthetic providers;
the runtime-free TUI passes 342 tests and warning-denying Clippy. An isolated copy of the
actual host provider wrapper passes three local-path metadata/budget/identity fixtures.
The complete stock-feature Nanus suite and workspace doctests were not run in this audit:
stock composition and the shipped secret-chain fixture can consult the user's native
credential stores, which a temporary `NANUS_HOME` does not isolate. These safe gates do not
substitute for that remaining hermetic full-workspace gate, immutable publication/adoption,
generic tool-support metadata, z.ai ceilings or live/platform acceptance. The host's published
pin, tool registry and production files remain unchanged. Source hashes and actual failed,
corrected and final logs were kept locally, outside the repository.

**Additional generic seam, proposed:** add a defaulted local
`LlmPort::tool_call_support(model, request_effort)` query returning
`ToolCallSupport::{Supported, Unsupported, Unknown}`. The adapter resolves the optional request
effort against its actual configured default and keys the answer by exact model, vendor,
endpoint and resolved protocol. `Option::None` means no override; it is distinct from
`Some(ReasoningEffort::None)`. Unknown is the backward-compatible trait default. No credentials,
network, app templates, skills or Claude plugin metadata participate in this query.

Known unsupported combinations refuse requests containing tool definitions or tool-bearing
history before HTTP. Check actual assistant call/result replay, including requests whose current
tool list is empty; never erase history or tools to obtain a text-only request. Stock automatic
routing and valid text-only behavior remain unchanged; stock unknown combinations are not
silently declared Supported. An embedding host may require Supported before admitting a tool
registry and before every request. It must not silently alter protocol, effort or model to make
that check pass.

Independent fixtures must cover Sol exact Chat text acceptance and tool/replay refusal,
Sol Responses tool support, Luna Chat with explicit None versus inherited/default and other
efforts, endpoint/model/protocol changes, unknown ids/proxies, and effort overrides on the same
adapter. Image metadata is tested independently in both directions. Use fake credentials and
local recorded transports to assert refusal before any HTTP; retain live exact-wire follow-up
as a separate acceptance gate. This requirement does not introduce a skill/plugin layer or
change the seven-tool default registry.

#### Local generic tool-support implementation — 2026-10-03

The additional seam above is now implemented locally: `ToolCallSupport` and the defaulted
`LlmPort::tool_call_support(model, request_effort)` are exported by `nanus-ports`. Legacy/fake
ports default to Unknown. The query performs no I/O and contains no skills, templates,
plugin registry, provider composition or credential selection. Stock Unknown requests retain
existing behavior; a strict embedding host must require Supported itself.

Exact official-endpoint entries are independent of image profiles and text ceilings:

- DeepSeek Flash and V4 Pro support ordinary Chat function tools with their existing neutral
  effort mapping, including reasoning passback; their different image states are unchanged.
- OpenAI Astra, Sol and Luna and the three exact GPT-5.6 Sol/Terra/Luna ids support Responses
  with their advertised native effort values. Astra/Sol Chat is Unsupported. Luna Chat is
  Supported only when the actual inherited or explicit effort is `ReasoningEffort::None`.
  Older models' Chat, subscription endpoints, z.ai, custom ids and gateways stay Unknown.
  [Function-calling restrictions](https://developers.openai.com/api/docs/guides/function-calling)
  and each exact model contract are checked independently; no image proof is reused as tools proof.
- Anthropic Opus 5.5, Sonnet 5.5 and Fable 5.1 support ordinary Messages tool use with their
  advertised native effort levels. Literal None/Minimal is Unsupported for tool-bearing requests
  because the existing encoder otherwise coerces those controls to low; this does not claim
  Anthropic low effort is unsupported. Existing text-only mapping is preserved. Forced tool
  choice is not emitted. Custom gateways now inherit neither official image/text metadata nor
  tool support. [Messages tool contract](https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls).

Concrete OpenAI/Anthropic estimation and streaming refuse known Unsupported definitions,
assistant calls and tool results before HTTP, even with an empty current tool list. Valid signed
Messages replay must agree exactly with neutral calls before its original blocks are emitted;
it cannot hide executable calls from this check. Output/context/image validation stays separate.
No protocol, selected model, default effort or seven-tool registry change is made.

Fourteen new fixtures pass within 262 affected-crate tests and one port doctest. Final rustfmt,
warning-denying workspace Clippy, minimal-bundle lint/139 tests/two doctests, runtime-free TUI
lint/342 tests and locked standalone consumers (seven without providers, ten with synthetic
providers) pass. The isolated host wrapper spike passes three prior metadata fixtures and one
new four-model forwarding/replay/selection fixture after explicit temporary admission and
query-delegation changes. It is a prototype, not a production-source or immutable-pin update;
the host's current wrapper would otherwise retain the legacy Unknown trait default.

Initial lifetime/constructor-order lint failures were corrected without suppression, and the
Anthropic no-contact fixture now has a timeout. Actual failed and final logs, guarded source
baselines and the isolated host copy were kept locally, outside the repository.
The complete stock-feature suite and workspace doctests remain explicitly unrun because their
native credential lookups are not isolated by `NANUS_HOME`. Native Windows, exact live-wire
acceptance, z.ai ceilings, published immutable adoption and app `read_video` ports remain gates.

#### N4 image profiles and capability promotion

These are the required initial implementation profiles, not a claim that current Nanus can send pixels. Each is keyed by exact model, adapter vendor and protocol. Before evidence is checked in, `image_input=Unknown` and `image_profile=None`; only the listed profile may become Supported after its gate passes. A returned Supported capability must contain matching validated limits/estimation metadata. Unknown/Unsupported always refuses image content. Other model IDs require a separate tested profile change rather than inheriting provider-wide support.

The [current model catalogue](https://platform.claude.com/docs/en/models/overview) names `claude-opus-5-5` and `claude-sonnet-5-5`; both have a 1M-token context and 128K-token standard maximum output. Implement their exact catalogue/ceiling entries rather than aliasing them to Opus/Sonnet 5 or retaining Haiku’s limits. The inspected adapter predates these entries; profile promotion also requires ordinary text/tool request and replay compatibility for each 5.5 model, including its adaptive-thinking contract. Existing models keep their own limits; this proposal does not claim these models are implemented today.

For both models, the [Anthropic vision contract](https://platform.claude.com/docs/en/build-with-claude/vision) specifies automatic high-resolution input (no beta opt-in), a 2576-pixel native long edge and 4784 visual tokens, with 28×28 patches. The API accepts PNG/JPEG/GIF/WebP and base64/URL/Files sources; this Nanus change implements inline PNG/JPEG only. Direct-API outer limits are 8000×8000 pixels, 10 MB base64 per image, 32 MB standard request and up to 600 images for these models. Requests with more than 20 images impose tighter dimensions; computer/browser-use screenshots over native limits are rejected rather than resized. These provider ceilings do not replace Nanus’s stricter byte/count bounds.

| Exact model / adapter / protocol                             | Initial profile                                                                                                         | Image charge before safety margin                             | Capability gate                                                                                                                                                                                           |
| ------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `claude-opus-5-5` / Anthropic / Messages                     | `anthropic-opus55-high-patch28-v1`: PNG/JPEG, dimensions ≥1, long edge ≤2576, visual patches ≤4784, file bytes ≤512 KiB | `ceil(width/28) × ceil(height/28)`; refuse a value above 4784 | Captured nested tool-result request, PNG/JPEG decode equality, native-limit rejection, multi-call ordering, v2 reload and one recorded live tool/image follow-up accepted by this exact model             |
| `claude-sonnet-5-5` / Anthropic / Messages                   | `anthropic-sonnet55-high-patch28-v1`: same bounded Anthropic high-resolution profile, independently keyed and verified  | `ceil(width/28) × ceil(height/28)`; refuse a value above 4784 | The same fixture/reload/native-limit and live acceptance gates, independently passed for Sonnet 5.5                                                                                                       |
| `gpt-6-astra` / OpenAI / Chat Completions                    | `openai-astra-high-patch32-v1`: PNG/JPEG, both dimensions 1–1024, file bytes ≤512 KiB; fixed `detail=high`              | `ceil(6 × ceil(width/32) × ceil(height/32) / 5)`              | Captured grouped tool-results then labelled user attachments, no duplicated/orphaned results, PNG/JPEG decode equality, v2 reload and one recorded live tool/image follow-up accepted by this exact model |
| OpenAI Responses; DeepSeek; z.ai; every other model/protocol | No profile in this change                                                                                               | No guessed fallback                                           | Unknown/Unsupported; image input fails before HTTP, text regressions still pass                                                                                                                           |

The selected bounded sizes fit the documented patch regimes without relying on provider resizing. Charge each image `ceil(5 × (base_charge + 32) / 4)`: 32 tokens for image framing and 25% safety headroom are library policy, not billed image tokens. At 1024×1024, the reserved charges remain 1752 for either Anthropic model and 1577 for OpenAI. At the Anthropic native maximum of 4784 visual patches (for example, 2576×1456), reserve 6020 tokens. Reject an overlong edge or excessive patch count before HTTP even if compressed bytes fit; the embedding host still supplies inspection images within its separate 1024×1024 app bound. Image header dimensions must agree with successfully decoded media; small compressed files with excessive dimensions are refused. The library does not resize or crop; callers must provide a bounded inspection image, retaining original full-resolution files separately. These formulas are based on the current [Anthropic vision contract](https://platform.claude.com/docs/en/build-with-claude/vision) and [OpenAI image-input contract](https://developers.openai.com/api/docs/guides/images-vision), retrieved 2026-10-01; the per-profile restrictions, byte/count caps and safety margin are library policy. Native-size and padding interpretation follows [Anthropic’s coordinate/resizing guide](https://platform.claude.com/docs/en/build-with-claude/vision-coordinates); the caller supplies an image already inside both native bounds, so neither encoder nor provider resizing is needed for a supported request.

For each claimed profile, check in evidence naming the exact model/protocol, Nanus revision, profile version, primary contract/date, request-fixture digest and successful live verification date. Fixture tests decode captured pixels and compare call/result/attachment order both fresh and after store reload. Live verification uses a fictional tiny image with an objective expected feature and a second turn that references the same call; do not infer acceptance from HTTP status alone. Missing credentials leaves the profile Unknown and its live gate unpassed. A contract/profile change invalidates prior evidence; a model/protocol switch rechecks every retained image against the new profile before request assembly. Store bodies remain model-neutral typed content; attachment messages are derived on each request and are never appended as extra user turns to the durable log.

`ModelCapabilities` carries optional `image_profile`, `context_window_tokens`, `max_input_tokens` and `max_output_tokens` beside `image_input`. `ImageProfile` is local validated metadata (profile/version, pixel/byte/count/request limits and checked dimension-to-token estimator), not a serialised tool schema. The configured request’s maximum output plus any separately bounded reasoning must fit the metadata; for the embedding host the combined reservation is explicitly set to at most 8192 tokens. Preflight requires estimated input ≤model input ceiling and input + reservation ≤min(caller context budget, model context ceiling); no automatically substituted output budget may bypass this check. Existing fake adapters default to unknown capabilities; stock text-only behavior does not acquire new mandatory metadata by accident.

### N5. Sessions → What a session is; What a session says about itself; Resuming (Modify)

> Session body format version 2 adds optional typed `content_blocks` to tool-result records. The reader accepts versions 1 and 2, normalises a version-1 result's text into legacy text semantics and preserves all original sequence numbers, call IDs, usage, goal and provenance fields. A version-1 record cannot introduce image blocks by smuggling an unknown field. Writers emit version 2; a prior binary cannot read these new logs, and the downgrade limit is documented. There is no bulk destructive rewrite of existing logs.
>
> Image-bearing version-2 sessions round-trip without degrading content and resume against a capable model. Images are bounded inline data rather than links to a user's original files, so moving a session does not depend on those files existing. Older logs have no inferred pixels or capability/provenance defaults. The store rejects oversized records before parsing, with a bounded total log read (64 MiB default), and leaves the existing log intact on a failed save. The TUI/link continue to transport summaries/live activity rather than duplicating image blobs across every client.
>
> `AgentRunner::run_turn` still mutates an in-memory session; it does not own persistence. CLI/link callers keep their existing persist-before-answer/Done behavior. Embedding examples explicitly save before acknowledging success. Interrupted side effects are not replayed from a transcript. Any new matches in TUI/link projections are exhaustive and preserve their current transcript display semantics.

### N6. Status → Known limits; Safety → Defaults, and what they do not protect you from (Modify)

> The minimal embedded runner subset supports macOS and Windows through caller adapters; the shipped stock shell/link/service remain Unix-only. The host's optional ToolPolicy adds exact-call checks, but filesystem and process containment remain properties of the supplied ports/OS. The default no-host-policy behavior and explicit permissive approval modes are unchanged.
>
> Wakeable cancellation ends a controlled turn while it is waiting, and callers remain responsible for tearing down detached process work. Vision support is explicitly listed by tested provider/model/protocol, with unsupported image input refused before network access. Prompt templates are plain caller-owned instructions; no Claude plugin compatibility is introduced.

### N7. Testing → The tests that matter most (Add)

> The minimal-library feature matrix and its runnable embedding doctest prove the runner works with caller ports on macOS/Windows without stock Unix adapters. Argument-aware policy tests cover exact IDs/arguments, read denial, default delegation, one-call write grant, policy error/cancellation, unknown tools and concurrency order. Cancellation tests use a never-ready model stream and paused policy/approval/tool futures; no wall-clock sleep or provider key is required to demonstrate a wake.
>
> Multimodal tests assert decoded PNG/JPEG bytes in captured provider requests, not merely a called `read_image` tool. They round-trip version-2 logs before reissuing requests, load version-1 fixtures unchanged in meaning, exercise all content bounds and verify text/call pairing on all adapters. Unsupported image models issue no HTTP request. The existing seven-tool default registry, five internal goal tools, wire allowlist, usage/provenance, persist-before-Done, process-group and CLI/TUI separation checks remain in force.

## Type changes

Nanus has no canonical JSON Schema sidecar today. The Rust domain/port definitions and tested serializers remain the boundary authority. This inline fragment documents the added wire/persistence fields, not a parallel repository-wide schema. Existing SessionEvent/Message fields remain; add `content_blocks` as described in N4 and N5. `ToolPolicy`/`TurnControl` are local Rust traits, never serialised tool schemas.

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$defs": {
    "ImageInputSupport": { "enum": ["supported", "unsupported", "unknown"] },
    "ToolPolicyDecision": {
      "oneOf": [
        {
          "type": "object",
          "required": ["kind"],
          "additionalProperties": false,
          "properties": { "kind": { "const": "use_default" } }
        },
        {
          "type": "object",
          "required": ["kind"],
          "additionalProperties": false,
          "properties": { "kind": { "const": "allow_once" } }
        },
        {
          "type": "object",
          "required": ["kind", "reason"],
          "additionalProperties": false,
          "properties": {
            "kind": { "const": "deny" },
            "reason": { "type": "string", "minLength": 1, "maxLength": 4096 }
          }
        }
      ]
    },
    "ContentBlock": {
      "oneOf": [
        {
          "type": "object",
          "required": ["type", "text"],
          "additionalProperties": false,
          "properties": { "type": { "const": "text" }, "text": { "type": "string" } }
        },
        {
          "type": "object",
          "required": ["type", "media_type", "data_base64"],
          "additionalProperties": false,
          "properties": {
            "type": { "const": "image" },
            "media_type": { "enum": ["image/png", "image/jpeg"] },
            "data_base64": { "type": "string", "minLength": 1, "maxLength": 699052 }
          }
        }
      ]
    },
    "ToolResultContent": {
      "type": "object",
      "required": ["content"],
      "additionalProperties": false,
      "properties": {
        "content": { "type": "string" },
        "content_blocks": {
          "type": "array",
          "minItems": 1,
          "maxItems": 32,
          "items": { "$ref": "#/$defs/ContentBlock" }
        }
      }
    }
  }
}
```

`ToolResultContent` is the changed content projection of existing `SessionEvent::ToolResult`/`Message::Tool`, not a replacement for call ID/error fields. Maximum 32 total blocks, four image blocks, 4 MiB record bytes and decoded-media checks apply in Rust as well as structural validation. `ModelCapabilities` and `ImageProfile` are local Rust metadata as specified in N4; the capability is not persisted as a guessed property of old sessions. Image charges are derived from validated pixels, never base64 length.

Proposed local API contracts (names are acceptance targets, not claims that these symbols already exist):

```rust
trait ToolPolicy {
    fn decide<'a>(&'a self, call: &'a ToolCall, access: ToolAccess)
        -> LocalBoxFuture<'a, Result<ToolPolicyDecision, PolicyError>>;
}
trait TurnControl {
    fn is_cancelled(&self) -> bool;
    fn cancelled(&self) -> LocalBoxFuture<'_, ()>;
}
```

Use an optional policy set during runner construction through a builder or constructor companion without breaking `AgentRunner::new` call sites. `run_turn_with_control` accepts `&dyn TurnControl`; the concrete embedding host supplies the cancellation primitive. The traits are dyn-compatible and do not require Send futures.

## Implementation notes

Baseline: local working copy was clean, parent `d1ee7deb80f6c82f2d2d67d0f4c6cf8e8a00cb6f`, inspected on 2026-09-30. CodeGraph located the call chain; its agent_loop index was stale, so current on-disk source was read before treating its line pointers as evidence.

| Current entry point                                                                                                                        | Work                                                                                                      |
| ------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------- |
| `crates/nanus-bundle/Cargo.toml`; `src/lib.rs:67`; `compose.rs`; `provider.rs`; `authorize.rs`; `selection.rs`; `error.rs`                 | Add `stock-compose` default feature, optional concrete dependencies and complete cfg/re-export/test audit |
| `crates/nanus-bundle/src/agent_loop.rs:328` (`AgentRunner::new`), `:538` (`run_turn`), `:808` (`run_tools`), `:949` (`gate`)               | Optional policy and wakeable controlled entry point; validate before grant; terminal result pairing       |
| `crates/nanus-bundle/src/agent_loop.rs:1003` (`render_content`); `tools/read.rs`                                                           | Retain display helper, preserve typed results, bound images before insertion                              |
| `crates/nanus-domain/src/tool.rs:271` (`ContentBlock`); `message.rs:244` (`Message`); `session.rs:280`, `:388`, `:1031`                    | Typed content, version-1/2 reader, fold, custom serializers and record validation                         |
| `crates/nanus-domain/src/context.rs:103` (`estimate_message`); `prompt.rs`                                                                 | Image estimate/byte bounds, whole-turn elision and no orphaned tool calls                                 |
| `crates/nanus-ports/src/llm.rs:47` (`LlmPort`), `lib.rs`; new `control.rs` if needed                                                       | Capability default and local cancellation trait; public docs/re-exports                                   |
| `crates/nanus-adapter-anthropic/src/wire.rs:103`; `nanus-adapter-openai/src/wire.rs`, `responses.rs`; `nanus-adapter-deepseek/src/wire.rs` | Provider encoding, explicit unsupported path, exhaustive message matches                                  |
| `crates/nanus-adapter-store/src/store.rs`; `nanus-tui/src/`; `nanus-link/src/server.rs`                                                    | Read/save bounds, log compatibility, text-only display projection, persist-before-Done regression checks  |
| `crates/nanus-bundle/tests/composition.rs`, `end_to_end.rs`, `live_wire.rs`; adapter live-wire tests                                       | Feature-specific helpers and controlled/multimodal integration fixtures                                   |

The consumer's own proposal, to embed Nanus in a Rust agent host, lives outside this repository. Nanus owns reusable runner/policy/cancellation/content capability; the host owns the skill prompt template, questions, fetch/search, app-specific tool executors, keychain, lifecycle, recovery and its UI channels.

Implementation order: feature isolation and a Windows minimal compile fixture; ToolPolicy/TurnControl with fake ports; typed content + dual-version session reader; exact-profile image encoders/context preflight/capability refusal; renderer/projection regressions and docs. The app consumes the resulting immutable tested Nanus revision. Local path dependencies are only for the development spike.

Provider wire details must be verified against current primary contracts: [Anthropic vision](https://platform.claude.com/docs/en/build-with-claude/vision), [OpenAI image input](https://developers.openai.com/api/docs/guides/images-vision) and the applicable endpoint's tool-result schema. Captured request tests must prove the chosen multimodal mapping is accepted, including an OpenAI tool result followed by user image input. Static shape assumptions do not establish live acceptance.

## Acceptance criteria

### Local caller-owned video snapshots — 2026-10-03

The optional video crate's source port now has an external construction seam:
`Snapshot::from_owned_file` checks a caller-verified fixed-name absolute source receipt
and retains an opaque thread-safe owner; `retain_owner` lets physical decoder workers
retain it through teardown. The constructor performs no I/O and does not authenticate
the file or its scope. Stock TempDir cleanup now uses the same final-reference lifetime,
with existing source copy/hash/bounds unchanged. This closes the private cleanup-field
barrier to caller `VideoSource` implementations; it does not supply the host's actual
bounded media adapter, decoder, routing, approvals or image/result accounting.
See the [video proposal](2026-10-01-read_video_extension.md#caller-owned-snapshot-seam--2026-10-03)
and [certificate](2026-10-03-video_snapshot.review.md) for demonstrated checks and limits.
The seam remains local unpublished work; no skill/plugin knowledge or downstream pin change.

### Local z.ai API admission implementation — 2026-10-03

The companion adapter now supplies exact Chat API metadata for `glm-5.3-flashx`,
`glm-5.3-flash`, `glm-5.3` and `glm-5.2` only at
`https://api.z.ai/api/paas/v4` (optional trailing slash). Context/input metadata uses
the conservative decimal interpretation 1,000,000 of the advertised 1M context;
output uses the API's explicit 1..131,072 range. Ordinary function-tool support is
independent of image metadata. Text GLM-5.3/5.2 images are Unsupported; Flash/FlashX
images remain Unknown without a profile or live capability promotion.

The [thinking contract](https://docs.z.ai/guides/capabilities/thinking) distinguishes
API requests from Coding Plan: the GLM-5.3 variants' API accepts only low/high/max;
GLM-5.2 accepts all seven neutral spellings, including none/minimal with thinking
enabled. The existing literal encoder and enabled switch remain correct; do not
infer the Coding Plan's server mappings on the API. Exact API effort lists and
default max now come from the configured endpoint/model; stock bundle selection
retains that valid default without overriding explicit configuration. Coding Plan,
custom endpoints, different vendors and unknown ids inherit no API evidence and
retain their existing defaults/behavior. Endpoint edits preserve captured effort;
request admission revalidates it and fails visibly if the new API rejects it.

Before HTTP, known API text and tool requests validate effort, zero/excess output,
the 128-function limit and actual serialized input plus output/reasoning reservation
against model/caller/record bounds, even without an explicit caller budget. Estimates
remain available for whole-turn elision; the stream checks fit before dispatch.
The [API reference](https://docs.z.ai/api-reference/llm/chat-completion) and exact
model pages provide contract evidence; local fixtures use fictional keys and loopback
transport only. The [semi-formal certificate](2026-10-03-zai_api_admission.review.md)
records source resolution, corrected findings, tests and remaining limitations.

This is local unpublished functionality. The host still consumes its earlier immutable
revision and has not adopted these metadata/admission APIs. Live provider wire/tool
follow-ups, image profiles, native Windows evidence and original embedding acceptance
remain open. No skill/plugin/template concept, toolset count or stock video authority
is added to Nanus by this delta.

1. `cargo tree -p nanus-bundle --no-default-features` contains no concrete adapter/CLI/TUI/link dependency. A minimal downstream fixture with caller fake ports builds/tests on macOS and Windows; explicit provider adapter dependencies also compile on both. Default Unix CLI/TUI/service still run through the original composition path.
2. Optional policy sees every registered tool with exact arguments before execution. Denial of an in-scope Read prevents I/O; AllowOnce runs exactly the given write; UseDefault preserves every approval mode; errors/unknown/malformed calls cannot bypass validation. No stock tool-count change or truthful-access-class rewrite is needed.
3. A cancellation signal wakes a never-ready stream, pending policy/approval and tool future. No newly approved effect starts after cancellation. Exactly one interrupted terminal result is produced; unfinished calls cannot poison a subsequent resumed request. Fake port teardown is asserted separately from the turn's wake.
4. A scripted read_image turn emits actual decoded PNG/JPEG fixture pixels for all three initial N4 profiles, retaining grouped call pairing, attachment/text order and error flags before and after store reload. Record each required exact-model live follow-up before promoting its capability to Supported; absent live evidence remains Unknown. Test Anthropic high-resolution fixtures including 1920×1080 (2691 visual patches), 2576×1456 (4784) and over-bound cases; require independent Opus/Sonnet capability and live evidence. Test OpenAI Responses and other profile-less combinations for no image HTTP, and preserve existing subscription text flows. Unsupported/unknown model capability is refused explicitly before HTTP. All text-only adapters preserve current request outcomes.
5. Version-1 logs load with unchanged text/sequence/provenance/usage. Version-2 image logs round-trip; a failed/oversized save does not replace the original. Malformed base64, invalid media, excessive image/block/record/session size and context-fit failure are tested at boundaries without unbounded allocations. Validate each Anthropic 2576-pixel long-edge/4784-patch bound, OpenAI’s 1024-pixel bounds and accumulated eight-image/4 MiB request caps, dimension-magic agreement, checked formula/safety charges, reserved output/reasoning and current-turn no-fit rejection. Context elision removes the whole original call/result group and its derived attachments. Transcript/TUI/link output contains no base64 dump.
6. Run all existing repository gates: `cargo fmt --all --check`; `cargo clippy --workspace --all-targets --all-features`; `cargo nextest run --workspace --all-features`; `cargo test --workspace --doc`; the two `nanus-tui --no-default-features` gates in AGENTS.md. Add minimal-bundle no-default-feature lint/tests and downstream feature-matrix checks so the all-features run cannot hide a missing cfg boundary. Native Windows evidence applies to the supported library subset, not the Unix workspace as a whole.
7. Update docs only with demonstrated behavior and report live/provider/platform checks that were not run. The change remains Proposed/Accepted until these criteria pass; this drafting task changes no Rust code.

## Implementation evidence (2026-10-01)

The Rust delta is implemented on this branch. The proposal remains Proposed because the
exact-model live image follow-ups have no available credentials. Native Windows/macOS CI
passed both minimal and explicit-provider fixtures on revision `890888f8519e988dbd19505d7b63c60e11b9ecfb`. Built-in image capabilities therefore remain Unknown. Canonical docs
record the implemented seams and these limits; [evidence](../../docs/vision-evidence.md)
tracks the remaining gates. Wire/reload fixtures are not substituted for live evidence.
The clean-agent semi-formal review found three defects (text block separators, DeepSeek
output reservation and example save-before-acknowledgment); all were fixed and covered
by captured downstream assertions before push. Do not move this proposal to merged until
the recorded acceptance criteria are satisfied.

## Merge plan

1. Implement the accepted library delta and run its default/minimal feature matrices and provider/platform checks.
2. Apply N1–N7 to the named existing docs headings, remove superseded image-placeholder-only/model-limit claims where changed and retain limits still true for CLI/link/service. Preserve unrelated known documentation drift.
3. Align Rust serializers/contracts with the inline fragment and acceptance fixtures. There is no global canonical-schema file to regenerate; do not create one merely to copy these Rust types.
4. Update public API examples and AGENTS.md where feature/build or body-version instructions change; report the downgrade limit. Keep normal prompt-template text as the whole skill contract.
5. Set status Merged and date, move to `changes/merged/`, fix relative links and update `.specs/README.md`, `docs/README.md` and any downstream dependency/review links together. Record the tested revision for the app.

## Assumptions and open questions

**Assumptions**

- The embedding host supplies platform filesystem/process/secret ports; Windows support is required for that embedded subset, not the existing Unix link/service.
- The same owner controls both repositories and requested an upstream proposal here on 30 September 2026.
- Model capabilities and image/tool endpoint shapes require wire fixtures plus live verification; provider labels alone do not prove them.

**Decisions**

- _Dependency boundary._ **Optional stock composition, enabled by default.** Isolates embedding without breaking the existing command-line product.
- _Policy._ **Exact-call asynchronous policy before the default gate.** Makes read scopes/approvals enforceable without labelling shell/write executors Read.
- _Cancellation._ **A wakeable caller-controlled local future.** Preserves single-threaded composability and makes idle I/O interruptible.
- _Images._ **Typed bounded blocks and a dual-version session reader.** Keeps the pixels through replay while preserving old text sessions and guarding body semantics.
- _Anthropic models._ **Opus 5.5 and Sonnet 5.5 high-resolution profiles.** Updated from Haiku on the owner’s request on 1 October 2026 using the current model/vision contracts; exact IDs, native limits and independent evidence distinguish provider support from the app’s smaller inspection policy.
- _Vision profiles._ **Exact model/protocol profiles with recorded accepted-wire/reload evidence.** Chosen on 1 October 2026 to close the review’s capability ambiguity; unknown mappings stay disabled.
- _Image budget._ **Dimension-based patch charges, explicit output/reasoning reservation and bounded image requests.** Replaces the uncalibrated universal 8192-token image estimate while retaining explicit safety headroom.
- _Skills._ **Ordinary system-prompt templates supplied by callers.** Explicit user instruction: add no Claude plugin support.
- _App tools._ **Questions, web fetching/search and app-specific operations stay in the host.** They use the existing generic ToolExecutor seam and do not enlarge Nanus's seven-tool default set.

**Open questions**

- Are test credentials available for the three required N4 image profiles? Missing keys leave those capability promotions unpassed; automated captured-wire tests alone cannot establish live acceptance.
