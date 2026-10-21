# Change: Managed context on the Responses API, and a reservation that follows the model

**Status:** Implemented — 2026-10-21, with the deviations below, including one accepted benchmark deviation. Parts A and B are in, and every acceptance case has a passing test. Originally accepted after two spec
reviews (findings and owner decisions folded in)
reviews (findings and owner decisions folded in) · **Date:** 2026-10-21 · **Owner:** Ant Stanley · **Target:** `nanus-adapter-openai` (managed
preparation, Responses encoder and decoder, declared limits), `nanus-domain` (context policy,
session body version 6), `nanus-bundle` (managed reservation, policy recording, status), `nanus-cli` (reserve flag),
`nanus-tui` (activation warning, replay), `docs/context-management.md`, `docs/sessions.md`,
`docs/features.md`, `AGENTS.md`

This change has two parts.

**Part A.** OpenAI sessions will be able to use managed context. Today every request routed to the
Responses API is refused before HTTP. That covers every OpenAI model the stock binary offers:
automatic routing sends `gpt-5.6` and later there, and those are the only OpenAI ids in the table.
The `subscription` plan is Responses-only as well. The Responses wire will get the same managed
preparation the chat-completions wire has:

- one encode, with an estimate and a digest of exactly those bytes;
- the managed role grammar;
- pre-parse argument limits for the context tools.

This covers the API plan and the subscription plan. History is always sent **plain**: user and
assistant text, function calls and their outputs, with no encrypted reasoning, no stored response
and no `previous_response_id`. On the subscription plan, which refuses `max_output_tokens`, the
output reservation is enforced by nanus's own accounting and is not sent. The opt-in stateless
replay mode stays refused.

**Part B.** The managed output reservation will default to the selected model's declared output
ceiling instead of a fixed 4,096 tokens, and it will follow the model across switches. When a host
fits requests to a budget smaller than the model's window, the following reservation is the smaller
of the ceiling and a quarter of that budget, so it never consumes the input allowance. Model limits
(window, input ceiling, output ceiling) belong to the *model*. They are the same on every plan and
endpoint that serves it, however the access is paid for.

---

## Motivation

The first managed-context change refused Responses because a Responses continuation *can* carry
opaque items bound to an unedited prefix (`.specs/changes/2026-10-04-responses_replay_and_schema_policy.md:156`).
That reasoning applies to the stateless replay mode only. The ordinary Responses path already sends
plain history: `store: false`, no reasoning items, and function calls as `function_call` items keyed
by `call_id` (`crates/nanus-adapter-openai/src/responses.rs:30-221`). Nothing in that history is
signed or held on the server, so hiding fragments is as safe there as on chat completions. The
refusal now covers far more than intended. `provider = "openai"` from the CLI or the interface has
no managed path at all, and the only "Supported" OpenAI row needs an exact chat preference, which
only a library host can set.

The fixed 4,096-token reservation has been the wrong default on every path, and Responses makes
the cost visible. The reservation is sent as the request's output ceiling. On reasoning models that
ceiling includes reasoning tokens, so a managed step at `high` effort is cut short with
`response.incomplete` or a length finish, and the turn exits non-zero. An ordinary request on the
same plan sends the model's own ceiling (`use_model_output_ceiling`). Managed context should not
make a turn more likely to be truncated than legacy context. A fixed recorded number also cannot
serve a session that switches models, because the declared ceilings differ widely:

| Model | Declared output ceiling |
|---|---|
| DeepSeek | 393,216 |
| z.ai GLM | 131,072 |
| OpenAI | 128,000 |
| Anthropic | 128,000 (every offered model) |
| `mercury-2` | 50,000 |

---

## Baseline this builds on

- **Managed preparation** (`docs/context-management.md` → Providers): `LlmPort::managed_support`
  and `LlmPort::prepare_managed`. A `PreparedModelCall` owns its body, endpoint and credential and
  performs no I/O until `stream` is polled. Its estimate charges `MANAGED_BYTES_PER_TOKEN`.
- **The OpenAI managed module** (`crates/nanus-adapter-openai/src/managed.rs`):
  - `support` (`:59`) requires chat completions and no `account_id`.
  - `prepare` (`:163`) refuses any Responses route with `unsupported_mode`, checks the reservation
    (`check_output`, `:228`), validates the grammar (`validate_sequence`, `:262`), encodes once,
    and returns `PreparedChat`.
  - `PreparedChat` decodes with `ToolArgumentLimits::managed()`, and its `Debug` omits the
    credential and the body (`:107`).
- **The Responses encoder:**
  - System messages are joined into `instructions` (`responses.rs:91`).
  - The conversation is flat input items (`encode_items(.., original = false)`, `:122`), including
    `input_image` items and a trailing `user` item after image tool results (`:178-221`).
  - `max_output_tokens` is sent only when `OpenAiConfig::sends_output_ceiling()` is true
    (`responses.rs:56`, `config.rs:485`). That is false for the subscription plan (automatic
    preference, fallback protocol Responses) and true for the API plan.
- **Declared limits:** `OpenAiConfig::capabilities` (`config.rs:646`) declares limits for all six
  OpenAI ids, on both Responses endpoints and on the Responses wire only:
  - a 1,050,000-token window;
  - a 922,000-token input ceiling (1,050,000 for `gpt-6-astra`);
  - a 128,000-token output ceiling;
  - image input Supported with a profile.

  On chat completions it declares nothing.
- **The runner's budget and reservation:**
  - `AgentRunner::context_budget` (`crates/nanus-bundle/src/agent_loop.rs:478`) is the model's
    declared window only when the runner was built `with_model_context_budget`, and the configured
    `AgentConfig::context_budget` otherwise (the library default is `DEFAULT_CONTEXT_BUDGET`,
    64,000). Stock composition opts in only when the configuration sets no `context_budget`
    (`crates/nanus-bundle/src/compose.rs:1043-1048`). Image requests already reserve a quarter of a
    configured budget, capped at 16,000 (`:1050-1066`), because a model's whole ceiling would not
    fit a small budget beside any input.
  - The managed path reads `policy.output_reserve_tokens`: the activation check (`prepare.rs:173-181`),
    the input allowance (`:195`), `request.max_tokens` (`:210`), `request.context_budget` (`:214`)
    and the status payloads (`status_of`, `:545-575`, which has no capabilities in scope, and the
    unready status at `:621`). The link and interface only copy the status number
    (`crates/nanus-link/src/server/context.rs:194`, `crates/nanus-tui/src/runtime.rs:4649-4666`).
  - `dry_estimate` reports a `candidate_too_large` estimate as ready, so an allowance of zero shows
    as a ready session whose every step fails.
  - The estimator takes the reservation from `request.max_tokens` and reads the body only when that
    is `None` (`crates/nanus-ports/src/capabilities.rs:244`).
  - `fits` charges input plus reservation against min(budget, window) (`:159`).
- **The policy record:** `ContextPolicy { mode, output_reserve_tokens, capture_shell,
  policy_version }` (`crates/nanus-domain/src/context/managed/records.rs:51`) uses
  `deny_unknown_fields`, and its default reservation is `DEFAULT_OUTPUT_RESERVE_TOKENS = 4_096`
  (`limits.rs:15`). `validate` refuses a reservation of 0. The CLI starts from the recorded policy
  and overwrites the reservation from `--context-output-reserve` (`crates/nanus-cli/src/cli.rs:1064`).
  The link's `ContextAction::Managed` records `ContextPolicy { mode, ..current }`, the recorded or
  default policy (`crates/nanus-link/src/server/context.rs:656-660`).
- **Recording a policy:** `change_policy` (`crates/nanus-bundle/src/agent_loop/managed/settle.rs:254-312`)
  validates the policy, returns without writing when `current == policy` (`:267`), checks
  activation, and appends the `context/mode` record through the checkpoint.
- **Body versions** (`crates/nanus-domain/src/session.rs:73-115`, `:1109`): a body is written at the
  lowest version that holds it. Readers accept 1 to 5 (`NEWEST_SESSION_FORMAT_VERSION = 5`). A record
  a lower version cannot hold is refused below its version, on write and on read
  (`needs_version_five`, `:454`, `:1902`). Typed user content below version 4 is refused by the
  *presence* of the raw field, even a null (`validate_body_version`, `:2161-2185`). Compaction
  digests are pinned to the version-5 header whatever the body's version
  (`compaction_prefix_digest`, `:1394-1405`; `check_compaction_positions`, `:1997-1998`).
- **The tests and strings that pin today's refusal:**
  - `every_responses_route_is_unsupported_and_refused_before_any_request`
    (`crates/nanus-adapter-openai/src/managed_tests.rs:414`);
  - the activation warning in `crates/nanus-tui/src/context.rs:50` ("refuses unsupported model paths
    (including Responses)").

---

## Affected spec pages

The canonical description of nanus is `docs/` plus `AGENTS.md` (see `.specs/README.md`).

| Canonical page | Nature of change |
|---|---|
| [`docs/context-management.md`](../../docs/context-management.md) → Configuration, activation and limits | Modify: the reservation defaults to and follows the model's declared ceiling, bounded by a smaller budget; the enabling sentence names version 6 |
| [`docs/context-management.md`](../../docs/context-management.md) → Providers | Modify: Responses joins the supported paths; stateless replay stays refused |
| [`docs/context-management.md`](../../docs/context-management.md) → Provider matrix | Modify the three OpenAI rows |
| [`docs/context-management.md`](../../docs/context-management.md) → Providers | Add the subsection "The Responses wire" |
| [`docs/context-management.md`](../../docs/context-management.md) → Checkpoints and recovery (body versions) | Modify: readers accept 1 to 6; a managed session may be version 6; compaction digests stay at version 5 |
| [`docs/context-management.md`](../../docs/context-management.md) → What is and is not verified | Modify: Responses fixtures; the subscription caveat |
| [`docs/context-management.schema.json`](../../docs/context-management.schema.json) | Add `output_reserve_follows_model` to `ContextPolicy` |
| [`docs/sessions.md`](../../docs/sessions.md) → body versions | Modify: version 6 |
| [`docs/features.md`](../../docs/features.md) → OpenAI subscription paragraph | Modify: remove the stale "no image items" sentence |
| [`docs/features.md`](../../docs/features.md) → managed context paragraph | Modify the supported paths and the reserve flag |
| [`AGENTS.md`](../../AGENTS.md) → body-version invariant | Modify: version 6, readers accept 1 to 6 |
| [`AGENTS.md`](../../AGENTS.md) → "For OpenAI, Automatic routing follows the model" | Modify: limits per model; subscription reservation; "cannot inherit" scoped to image and tool evidence |
| [`AGENTS.md`](../../AGENTS.md) → "Managed context is opt-in" | Modify: the managed-session version wording includes 5 and 6 |

---

## Proposed changes

### `docs/context-management.md` → Configuration, activation and limits (Modify)

Replace the first paragraph with:

> `ContextPolicy` has five fields: `mode` (`legacy` | `managed`), `output_reserve_tokens`,
> `output_reserve_follows_model`, `capture_shell` and `policy_version`. The defaults are legacy, a
> following reservation, false and 1. A *following* reservation is resolved at each request from the
> model in force, so it moves with a model or provider switch:
>
> - when requests are fitted to the model's window (the stock default, with no `context_budget`
>   configured), it is the model's declared output ceiling;
> - when they are fitted to a smaller budget — a configured `context_budget`, or a library host's
>   default of 64,000 — it is the smaller of the ceiling and a quarter of that budget, as for image
>   requests, so at least three quarters of the budget stays for input.
>
> `output_reserve_tokens` then records the reservation resolved when the policy was recorded, for a
> reader, and is not otherwise used. An *explicit* reservation is the recorded number and does not
> move. Capture is valid only in managed mode, and nothing turns managed mode on implicitly.

In the next paragraph, replace "version 3, or version 4 marked managed when it holds typed user
content" with "version 3, or version 4, 5 or 6 marked managed; a default enable writes version 6,
because its policy follows the model".

In the activation paragraph, replace "when its declared output ceiling is below the reservation" with:

> when the reservation is explicit and the model's declared output ceiling is below it, or when the
> reservation follows the model and the model declares no output ceiling

Replace the `nanus run` sentence with:

> `nanus run` takes `--context-mode legacy|managed`, `--context-output-reserve <tokens>|model` and
> `--capture-shell-evidence`. `model` selects a following reservation and a number an explicit one.
> Absent flags keep a resumed session's recorded policy, or legacy for a new one. A session recorded
> with an explicit reservation keeps it until a human changes it; a new managed session follows the
> model. A capture or reserve flag on a new session without `--context-mode managed` is refused.

The input-allowance sentence after the limits table stays as it is. Add:

> With a following reservation and the model's window as the budget, the allowance is the window less
> the output ceiling. On every supported model that leaves at least 78,000 tokens (`mercury-2`:
> 128,000 − 50,000); for OpenAI it is 922,000, the declared input ceiling. With a smaller budget the
> allowance is at least three quarters of it.

### `docs/context-management.md` → Providers (Modify)

Replace the paragraph that begins "The supported matrix is recorded with the provider work" with:

> The supported matrix is recorded with the provider work (see [Providers](#provider-matrix) below).
> The chat-completions, Messages and Responses wires are supported at their official endpoints for
> known models. All of them send history plain. Anthropic's may also send signed replay, under the
> rule below. The opt-in stateless Responses replay mode stays unsupported. Activation and provider
> switches refuse it before HTTP, with no wire, model or effort switch, and no opaque item is
> deleted or rewritten.
>
> A model's limits — window, input ceiling and output ceiling — are the model's. They are declared
> once per model id and apply on every plan and endpoint that serves it, so a subscription has the
> same budget as API access to the same model.

### `docs/context-management.md` → Provider matrix (Modify)

Replace these three OpenAI rows:

> | OpenAI | API plan, `https://api.openai.com/v1`, with an exact Chat Completions preference | `openai.chat` | The offered models whose chat tool support is not refused (`gpt-6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`) | Supported |
> | OpenAI | Any id the vendor does not offer, which has no declared output ceiling | — | — | Unsupported |
> | OpenAI | Anything routed to Responses: automatic `gpt-5.6`+, the `subscription` plan, exact Responses, stateless Responses | — | All | Unsupported; refused before HTTP |

with:

> | OpenAI | API plan, `https://api.openai.com/v1`, exact Chat Completions preference | `openai.chat` | The offered models whose chat tool support is not refused (`gpt-6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`) | Supported |
> | OpenAI | API plan, `https://api.openai.com/v1`, automatic routing or an exact Responses preference | `openai.responses` | Every offered model | Supported |
> | OpenAI | `subscription` plan, `https://chatgpt.com/backend-api/codex`, automatic routing with the Responses fallback | `openai.responses` | Every offered model | Supported; the reservation is enforced by nanus's accounting and not sent |
> | OpenAI | `subscription` endpoint under any exact preference, or an automatic preference whose fallback is chat | — | All | Unsupported: an exact preference promises a ceiling the endpoint refuses, and the chat fallback would send one |
> | OpenAI | Stateless Responses replay | — | All | Unsupported; refused before HTTP |
> | OpenAI | Any id the vendor does not offer, or any other endpoint | — | — | Unsupported |

### `docs/context-management.md` → Providers → The Responses wire (Add)

Insert after the Anthropic signed-replay paragraph:

> #### The Responses wire
>
> A managed Responses request is encoded by the same encoder as an ordinary one, so the two differ
> only in what the managed compiler selected:
>
> - The leading system messages (the harness prompt, the notice, and a seeded session's handoff
>   seed) are joined in order into `instructions` and are not repeated in `input`.
> - A user turn is a `user` item, with `input_image` parts when it carries images.
> - An assistant turn is an `assistant` item of `output_text`, then one `function_call` item per
>   call, keyed by `call_id` and carrying no item id.
> - A result is a `function_call_output`. A result that carries images is answered with its label,
>   and the original text and pixels follow as one `user` item once the whole group of results has
>   been answered.
> - The generated working-data message is an `assistant` item directly after the first user item,
>   and never last.
>
> History is always plain. The body carries `store: false` and never `previous_response_id`,
> `include`, a reasoning item, or any other protocol's replay. An assistant turn recorded with replay
> blocks (Anthropic's, or stateless Responses items) is sent as its text and calls. A turn that only
> replay can express refuses preparation with `protocol_incompatible`, as it does on chat
> completions. Earlier reasoning is therefore never sent back to the model. The ordinary Responses
> path already works this way, so managed context costs no continuity the session had.
>
> On the API plan the reservation is sent as `max_output_tokens`. The `subscription` endpoint
> refuses that field, so there the reservation is enforced by nanus's accounting alone. It must
> still be present and within the model's declared ceiling. The estimator charges it against the
> budget as on every other path, and the input allowance is reduced by it. With the default
> following reservation and the model's window as the budget it is the model's whole ceiling, so
> nothing the backend can return exceeds what was reserved. A smaller reservation — explicit, or a
> following one bounded by a configured budget — is not sent either. A reply longer than it is
> recorded at its real size in the attempt's usage, and the next step's fit counts it as history.
> The subscription endpoint under an exact preference is unsupported, because an exact preference
> promises that a ceiling is sent and honoured.
>
> The decoder applies the context tools' raw argument limits on this wire too:
>
> - It checks a call once its name is known, whether the name arrives in
>   `response.output_item.added` or only in `response.output_item.done`.
> - Before the name is known, it bounds the buffer at one record.
> - A call over its limit keeps `OVERSIZED_ARGUMENTS`, and the whole arguments string an item's
>   `done` event carries cannot restore it.
>
> The selection identity names:
>
> - provider `openai`;
> - the digest of the exact `/responses` URL;
> - protocol `openai.responses`;
> - the model;
> - the effort the body's `reasoning.effort` carries.
>
> A prepared call holds the bearer token and account id captured when its adapter was built, as an
> ordinary dispatch does, and its `Debug` renders neither. A token that expires between
> composition and the send fails that attempt, and renewal happens when the adapter is next built.
>
> A managed session that moves between `openai.chat` and `openai.responses` needs no reset, because
> both send plain history. Stock routing never makes that move, since every offered id resolves to
> Responses. A library host can make it by preparing with a differently configured adapter. Each
> step's selection is recorded with its attempt.

### `docs/context-management.md` → Checkpoints and recovery (Modify)

In the body-version sentence, change "Readers accept versions 1 to 5" to "Readers accept versions 1
to 6". Add:

> Version 6 is the lowest version that holds a `context/mode` record whose policy follows the
> model. A reader that predates version 6 refuses such a session at the header, and the field's
> presence — with any value — is refused in any body below version 6, on write and on read. A
> compaction marker's prefix digest is computed under the version-5 header whatever version the
> body later reaches, so a compacted session that gains a version-6 record still validates.

In the paragraph on version 3, replace "version 3, or version 4 or 5 with `"managed":true` in its
header" with "version 3, or version 4, 5 or 6 with `"managed":true` in its header". A version-6
body always holds a `context/mode` record, which only `change_policy` writes after upgrading the
body, so its header always says `"managed":true`.

### `docs/context-management.md` → What is and is not verified (Modify)

- Change "the three adapters' encoders and decoders" to "the three adapter crates' encoders and
  decoders, including OpenAI's Responses wire".
- Add to the verified list: "the Responses managed body on the API and subscription plans, sent
  through the real adapter and runner to a local listener. The tests cover:
  - the order of `instructions`;
  - plain history, with no replay and no `include`;
  - `max_output_tokens`, present and equal to the reservation on the API plan, absent on the
    subscription plan;
  - images and image results;
  - exact bytes, and the same digest after a reload;
  - argument limits, including a name that arrives only at `output_item.done`;
  - a following reservation across a model switch."
- Add to the not-verified list: "**The subscription reservation is not enforced by the provider.**
  The tests show the reservation is charged and that the body omits it. The window the subscription
  is fitted to is the model's declared window, by decision, and has not been measured against the
  backend."

### `docs/context-management.schema.json` → `ContextPolicy` (Modify)

See [Type changes](#type-changes).

### `docs/sessions.md` → body versions (Modify)

Apply the same version-6 sentence as in context-management, and change "versions 1 to 5" to
"versions 1 to 6".

### `docs/features.md` → OpenAI subscription paragraph (Modify)

Remove "A conversation holding images stays on chat completions, because the Responses encoder has no
image items yet." The encoder sends `input_image` items, and routing does not consider images.

### `docs/features.md` → managed context paragraph (Modify)

> Opt in per session with `nanus run --context-mode managed` (and optionally `--context-output-reserve
> <tokens>|model` and `--capture-shell-evidence`) … Supported on DeepSeek, z.ai and Inception Chat
> Completions, OpenAI Chat Completions and Responses (the API and `subscription` plans), and
> Anthropic Messages, at their official endpoints. History is sent plain on every path except
> Anthropic's, which may also send signed replay. OpenAI's stateless replay mode refuses. The
> output reservation defaults to the model's declared ceiling and follows it across switches; with
> a configured `context_budget` smaller than the window it is at most a quarter of that budget.

### `AGENTS.md` → body-version invariant (Modify)

> **A body is written at the lowest version that holds it; readers accept 1 to 6.** Version 6 when a
> `context/mode` record's policy follows the model; version 5 when …

Apply the same pattern to the rest of the bullet: a version-6 body is refused at the header by an
older build, and the field is refused below version 6, on write and on read.

### `AGENTS.md` → "For OpenAI, Automatic routing follows the model" (Modify)

Append after "…which that backend refuses.":

> A model's window and ceilings are the model's on every plan and wire. A managed request on the
> subscription still carries its reservation in accounting: the reservation is checked against the
> declared ceiling and charged, but never sent.

Replace "Chat/proxy support cannot inherit Responses evidence" with "Chat/proxy image and tool support
cannot inherit Responses evidence; text limits are the model's".

### `AGENTS.md` → "Managed context is opt-in" (Modify)

Replace "A managed session (version 3, or version 4 marked managed)" with "A managed session
(version 3, or version 4, 5 or 6 marked managed)".

---

## Type changes

The `ContextPolicy` field is added. It is optional on the wire and written only when true.

```json
{
  "$comment": "Fragment for 2026-10-21-managed_context_responses. Folds into docs/context-management.schema.json on merge.",
  "$defs": {
    "ContextPolicy": {
      "type": "object",
      "additionalProperties": false,
      "required": ["mode", "output_reserve_tokens", "capture_shell", "policy_version"],
      "properties": {
        "mode": { "enum": ["legacy", "managed"] },
        "output_reserve_tokens": {
          "type": "integer", "minimum": 1, "maximum": 4294967295,
          "description": "Explicit reservation, or the reservation resolved when a following policy was recorded."
        },
        "output_reserve_follows_model": {
          "const": true,
          "description": "Added. Present only when the reservation follows the model in force; absent means explicit. Only a version-6 body may carry the key at all, and false is refused."
        },
        "capture_shell": { "type": "boolean" },
        "policy_version": { "const": 1 }
      }
    }
  }
}
```

`SelectionIdentity.protocol` stays a free string, and `openai.responses` needs no schema change. The
link's `ContextStatusInfo.output_reserve_tokens` keeps its shape (`u64`) and reports the *effective*
reservation (see B3), so `PROTOCOL_VERSION` does not move.

---

## Design

### A1. Support

`managed::support(config, model)` becomes a predicate over the resolved wire:

- **Chat completions:** unchanged.
- **Responses:** all of the following must hold:
  - `vendor == OpenAi` and `!config.stateless_responses()`;
  - `resolve_protocol(model) == Ok(Responses)`;
  - `model` is in `Vendor::OpenAi.models()`;
  - `capabilities(model)` declares a window and an output ceiling;
  - `tool_support::support(config, model, None) != Unsupported`;
  - and either of:
    - the base URL is `OPENAI_BASE_URL`, with no `account_id`, and `sends_output_ceiling()` is true;
    - the base URL is `OPENAI_SUBSCRIPTION_BASE_URL`, `protocol_preference()` is `Automatic`, and
      `sends_output_ceiling()` is false.

The subscription branch admits only the stock plan: an automatic preference whose fallback is
Responses. `sends_output_ceiling()` alone is not enough, because under an exact preference it is
false at the subscription URL (`config.rs:485-492`), and `checked_protocol` (`lib.rs:241`) then
refuses every request that carries `max_tokens` — which every managed request does. The branch
therefore refuses any exact preference there, and an automatic preference whose fallback is chat,
which would send a ceiling the backend refuses. z.ai and Inception never resolve to Responses, so
nothing changes for them.

### A2. Preparation

`prepare` keeps one sequence of checks and branches only where the wire differs:

1. Refuse stateless mode and anything `support` refuses (`unsupported_mode`), before any encoding.
2. Run the shared checks, unchanged: `checked_protocol`, `check_output` against
   `caps.max_output_tokens`, `validate_sequence`, `validate_image_input`, and
   `tool_support::validate_input`.
3. Encode once: `wire::build_request` for chat completions, `responses::build_request` for
   Responses. Both are the ordinary encoders. Responses must not call `encode_replay_input` or set
   `include`.
4. Run `estimate_managed_payload` and `fits`, serialise, and
   `assert_eq!(body.len(), estimate.request_bytes)`.
5. Build the transport from `llm.url(protocol)`, and the selection identity with the wire's
   protocol label.

`PreparedChat` becomes `PreparedCall { protocol, transport, body, estimate, digest, selection }`.
Its `stream` builds `Decoder::Chat` or `Decoder::Responses`, each with
`ToolArgumentLimits::managed()`. Its `Debug` keeps the current redaction.

Images are in scope. Managed preparation already admits them wherever `validate_image_input`
accepts the model's declared support. On Responses that is every offered id. On OpenAI chat it
stays Unknown, so it refuses.

### A3. Subscription accounting

The subscription path needs no new mechanism:

- The runner sets `max_tokens` (the effective reservation; see B) and `context_budget`.
- The estimator reads the reservation from `request.max_tokens`.
- `sends_output_ceiling()` omits the field on the stock subscription plan.
- `checked_protocol` refuses a request carrying `max_tokens` only under an exact Responses
  preference at the subscription URL, which A1's `Automatic` condition excludes, so no admitted
  managed request reaches that refusal.

The change admits the path, and tests pin that the reservation is charged and not sent.

### A4. Decoder limits

`responses::StreamAccumulator` gains `argument_limits: ToolArgumentLimits`, a per-call `oversized`
flag, and a `set_argument_limits` that mirrors `wire.rs:300`:

- Limits are checked at `output_item.added`, at each `function_call_arguments.delta`, and at
  `output_item.done`, by the rule at `wire.rs:454-473`.
- Once a call is flagged, `done` does not replace its arguments.
- `close` emits `OVERSIZED_ARGUMENTS` for a flagged call.
- Ordinary streams keep empty limits, so their decoding is unchanged byte for byte.

### A5. Limits per model on every wire

`OpenAiConfig::capabilities` declares the six offered ids' window, input ceiling and output ceiling
on both wires, at `OPENAI_BASE_URL` and at `OPENAI_SUBSCRIPTION_BASE_URL`. The image fields stay as
they are: Supported with a profile on Responses, and default (Unknown) on chat, where no image
evidence exists. Tool support is unchanged. The `openai.chat` managed path therefore has a declared
ceiling to follow.

This changes legacy behaviour for one configuration, and the change is intended — "the budget is the
model's":

- Stock is unaffected: every offered id resolves to Responses, which already declares these limits.
- A library host on an exact chat preference that opted into `with_model_context_budget` now fits to
  the model's window instead of its configured budget. Its legacy `prompt_budget` moves from the
  configured 64,000 to 691,500 (the 922,000 input ceiling less a quarter), and admission's caps move
  with it.
- `max_completion_tokens` is unchanged at 128,000: `output_ceiling` already reads the per-model
  table on every wire.
- Image support on chat stays Unknown, so an image conversation on exact chat still refuses.

The comment on `capabilities` (`config.rs:641-645`, "Chat and proxies inherit nothing") becomes
"Chat inherits the model's text limits and no image or tool evidence; proxies inherit nothing". The
assertion that exact chat declares default capabilities
(`crates/nanus-adapter-openai/tests/protocol_preference.rs:140-146`) flips to the declared text
limits with image support Unknown.

### B1. The policy

`ContextPolicy` gains `output_reserve_follows_model: bool`, serialised only when true
(`skip_serializing_if = "is_false"`) and read by a `deserialize_with` that accepts an absent field as
false and a present `true`, and refuses a present `false` or any other value. The writer never emits
`false`, so a reader that met one would be reading a body an older build cannot open; the schema's
`const: true` says the same.

- `Default` and `managed()` set it true. `Default` keeps `DEFAULT_OUTPUT_RESERVE_TOKENS` (4,096) in
  `output_reserve_tokens` as an unresolved placeholder, so `validate` (which refuses 0) still holds
  for a policy no model has resolved. The constant is therefore kept, and its doc comment says it is
  the placeholder of a following policy, never a reservation in force.
- A record written before this change deserialises false, which is what that record meant, because
  it named a number.
- `disabled()` keeps the field, as it keeps the number.
- A new type, `ReserveBudget`, says what a request is fitted to: `ModelWindow` or `Configured(u32)`.
  It is a domain type because the domain cannot see `ModelCapabilities`.
- A new method, `effective_reservation(ceiling: Option<u32>, budget: ReserveBudget) -> Option<u32>`:
  - explicit: `Some(output_reserve_tokens)`, whatever the arguments;
  - following, no ceiling: `None`;
  - following, `ModelWindow`: `Some(ceiling)`;
  - following, `Configured(fit)`: `Some(ceiling.min(fit / 4).max(1))`, using `checked_div`.
- A new method, `same_settings(&self, other) -> bool`, compares mode, capture, policy version and
  the follows flag, and the number only when both are explicit. Two following policies whose
  resolved snapshots differ are the same setting.

### B2. Body version 6

- Add `SESSION_FORMAT_VERSION_RESERVE_FOLLOWS_MODEL = 6` and make it `NEWEST_SESSION_FORMAT_VERSION`.
- Add `SessionEvent::needs_version_six`, true for a `context/mode` record whose policy follows.
- `body_version` returns 6 when any record needs it, ahead of the version-5 rule.
- Version 6 holds everything version 5 holds. The store's listing asks `reads_session_version`, which
  now accepts 1 to 6.
- Below version 6 the field is refused by its raw *presence* in a `context/mode` event's policy,
  whatever its value, on write and on read. The check reads the raw line, like
  `validate_body_version` (`session.rs:2161-2185`), but as its own function, because that one
  returns early for every version from 4 up. The refusal names the version whatever the value, so
  a present `false` in a version-5 body is reported as a field that version cannot hold.
- The header's `managed` flag means at version 6 what it means at versions 4 and 5. A version-6
  body always holds a `context/mode` record, so the flag is always true there.
- Compaction digests stay pinned to `SESSION_FORMAT_VERSION_COMPACTED` (5). The doc comment on
  `compaction_prefix_digest` (`session.rs:1394-1397`) changes "It equals `Session::prefix_digest`
  once the marker is in the log" to "It equals `Session::prefix_digest` while the body is at
  version 5; a later version-6 record does not change it". A session managed once and switched back
  to legacy may be compacted and then gain a following record (for example from
  `--context-output-reserve model` on a resumed session), and its marker must still validate.
- A version-6 record on a disabled policy changes no behaviour: legacy mode reads no reservation.

### B3. The runner

The runner resolves the reservation once per preparation, in one method beside `input_allowance`:

```text
fn managed_reservation(&self, caps, policy) -> Option<u32>
    fit = self.context_budget().min(caps.context_window_tokens or MAX)
    budget = if caps.context_window_tokens == Some(fit) { ModelWindow } else { Configured(fit) }
    policy.effective_reservation(caps.max_output_tokens, budget)
```

A budget at or above the declared window is the window, whether it came from
`with_model_context_budget` or from a large configured number. A model that declares no window is
fitted to the configured budget and is always `Configured`.

It is used everywhere the managed path reads `output_reserve_tokens` today:

- In `unready` (`:173-181`): `None` returns `unsupported_mode`. An explicit policy keeps the existing
  ceiling check. A following one passes it by construction, since it never exceeds the ceiling.
- In `input_allowance` (`:195`), `request.max_tokens` (`:210`) and both status payloads.

`dry_estimate`'s reading of `candidate_too_large` as ready is unchanged. A following reservation
leaves at least three quarters of a configured budget, or the window less the ceiling, so it cannot
produce a zero allowance by itself. An explicit reservation at or above the budget still can, as it
can today.

**Recording.** `change_policy` (`settle.rs:254-312`) resolves a following policy before it records
it. It reads the selected model's capabilities, computes `managed_reservation`, and writes the
result into `output_reserve_tokens`. For a managed policy it refuses with `unsupported_mode` when
that is `None`. For a legacy policy that is `None`, the number is left as it is. The no-op test at
`:267` becomes `current.same_settings(&policy)`, so recording an unchanged following policy after a
model switch writes nothing.

**Status.** `status_of` (`prepare.rs:545-575`) gains the effective reservation as an argument, since
it has no capabilities in scope. The unready status at `:621` computes it the same way. Both report:

- the effective reservation when there is one, for either mode, so a legacy session's status shows
  what enabling would reserve;
- otherwise (a following policy on a model that declares no ceiling), the recorded
  `output_reserve_tokens`, with `managed_ready: false` and `unavailable_reason: unsupported_mode`
  when managed.

The status types stay `u64` (`crates/nanus-domain/src/context/managed/tooling.rs:300`,
`crates/nanus-link/src/protocol.rs:397`). The link and the interface copy the number and need no
change.

**Handoff.** Handoff drafts reach the same reservation through `prepare_maintenance`. The handoff
document's policy line (`crates/nanus-bundle/src/agent_loop/managed/handoff.rs:276-285`) renders
`output_reserve=model` for a following policy and the number for an explicit one.

A model or provider switch needs no new record. The next preparation reads the new model's ceiling
and the runner's budget.

### B4. Clients

- **CLI** (`crates/nanus-cli/src/cli.rs:112`, `:1064`): `--context-output-reserve` accepts a
  positive integer or `model`, parsed into a `ReserveFlag { Tokens(u32), Model }` by a custom
  `value_parser` that replaces today's `u32` range parser (`:111`), with `value_name` `TOKENS|model`. A number sets
  `follows = false` and the number. `model` sets `follows = true` and leaves the number for
  `change_policy` to resolve. The flag is still refused on a new session without
  `--context-mode managed`, at both sites (`:690` and `:1080`). The help text names both forms, and
  the parser tests (`:1995-2034`) gain `model` and the refusals.
- **Link:** no code change. `ContextAction::Managed` records `{ mode, ..current }`
  (`server/context.rs:656-660`). A session with no recorded policy starts from `Default`, so a session
  enabled from the interface follows the model. A session with an explicit recorded reservation keeps
  it. The status frame reports the effective number.
- **Interface:** `crates/nanus-tui/src/context.rs:50` drops "(including Responses)". The sentence
  becomes "The agent refuses model paths without managed support, and an explicit reserve above the
  model's ceiling." `crates/nanus-tui/src/replay.rs:234` renders a following recorded policy's
  reserve as `model`, so the line reads "context: managed (output reserve model, shell capture off)".

### Still refused

- Stateless Responses replay.
- The subscription endpoint under any exact preference, or an automatic preference whose fallback
  is chat.
- Unknown ids, other endpoints and proxies.
- A turn only replay can express.
- A following reservation on a model that declares no output ceiling.

Every refusal happens before HTTP, with no wire, model or effort switch.

---

## Implementation notes

```
Domain (B1, B2)
1. crates/nanus-domain/src/context/managed/records.rs:51-109 — field with a deserialize_with that
   refuses false, Default/managed/disabled, ReserveBudget, effective_reservation, same_settings;
   limits.rs:15 constant kept as the following placeholder, doc comment rewritten.
2. crates/nanus-domain/src/session.rs:73-115, :454, :1109, :1902 — version 6 constant, newest,
   needs_version_six, body_version, write/read refusal below 6; a raw-presence check beside
   validate_body_version (:2161-2185); the compaction_prefix_digest doc comment (:1394-1397).
3. docs/context-management.schema.json — ContextPolicy property; context_schema.rs checks it.

Adapter (A1–A5)
4. crates/nanus-adapter-openai/src/config.rs:641-646 — declare text limits on both wires (A5);
   rewrite the comment. Flip tests/protocol_preference.rs:140-146.
5. crates/nanus-adapter-openai/src/responses.rs:258 — argument limits and oversized flag
   (:398 delta, :485 added, :513 done, :536 close), mirroring wire.rs:454-473 and :547.
6. crates/nanus-adapter-openai/src/managed.rs:59 — support() split into chat and Responses
   predicates; the subscription branch requires an Automatic preference.
7. crates/nanus-adapter-openai/src/managed.rs:99-225 — PreparedChat → PreparedCall; prepare() branches
   encoder, URL, decoder and label ("openai.chat" / "openai.responses"); effort from
   body["reasoning"]["effort"] on Responses. Rewrite the module doc (:1-39).
8. crates/nanus-adapter-openai/src/managed_tests.rs:414 — split the refusal test. Keep the stateless,
   exact-subscription, chat-fallback-subscription and unknown-id refusals. The automatic API
   (:416-436), the subscription (:438-453) and the API endpoint under exact Responses (:455-463)
   flip to Supported and move to new tests.

Runner and clients (B3, B4)
9.  crates/nanus-bundle/src/agent_loop/managed/prepare.rs:173-214 — managed_reservation; unready,
    input_allowance, base_request. :545-575 status_of takes the reservation; :621 likewise.
10. crates/nanus-bundle/src/agent_loop/managed/settle.rs:254-312 — resolve a following policy before
    recording; same_settings replaces == at :267.
11. crates/nanus-bundle/src/agent_loop/managed/handoff.rs:276-285 — render `model`.
12. crates/nanus-cli/src/cli.rs:111-112, :690, :1064, :1080, :1995-2034 — ReserveFlag parsing,
    help text, both refusal sites, parser tests.
13. crates/nanus-tui/src/context.rs:50, crates/nanus-tui/src/replay.rs:234 — strings.

Tests and bench that read the old default (B1 changes what Default and managed() mean)
14. Every test that relies on a 4,096 reservation pins an explicit policy
    (`output_reserve_tokens: 4_096`, follows false) instead of `ContextPolicy::managed()` or
    `default()`; tests about following use the default. Today these name the constructors:
    the three adapters' managed_tests.rs; nanus-bundle tests maintenance.rs, managed_capture.rs,
    managed_context.rs and managed_context/harness.rs; nanus-domain compile/tests.rs, records.rs,
    tests/compaction.rs, tests/context_schema.rs, tests/handoff.rs, tests/user_content.rs;
    nanus-link server/context.rs, server.rs:3821 and tests/link.rs. A test whose session body
    version is asserted moves to 6 only where it enables managed through the default.
15. crates/nanus-bench/benches/managed_turn.rs:374 and crates/nanus-bench/examples/context_footprint.rs
    pin the explicit 4,096 policy, so the saved baselines stay comparable.

Docs
16. The blocks above, landing with the code (repository practice).
```

The Responses decoder is a hot path. Measure it with `cargo bench -p nanus-bench` against a baseline
saved on the parent revision (`docs/benchmarks.md`) and record the result with the change. An
ordinary stream should not move.

### Acceptance cases

Each case comes with its refusing counterpart, per the testing conventions.

**Adapter (`crates/nanus-adapter-openai/src/managed_tests.rs`, local listener)**

1. **The API plan sends exactly what it prepared.**
   - Setup: automatic `gpt-5.6-sol` and `gpt-6-astra`, and the API endpoint under exact Responses.
   - `managed_support` is Supported, and the listener receives `body()` byte for byte.
   - The body has `store: false`, `max_output_tokens` equal to the reservation, and `instructions`
     equal to the prompt and notice joined.
   - It has no `include`, no `previous_response_id`, and no reasoning-type input item.
2. **The subscription plan omits the reservation and still charges it.**
   - Setup: the stock subscription configuration with an `account_id`.
   - The body has no `max_output_tokens`, and `estimate().reservation` equals the reservation.
   - A candidate that fits only without the reservation is refused with `candidate_too_large`.
   - A reservation above 128,000 is refused, never clamped.
3. **History is plain.**
   - Setup: an Anthropic-replay assistant turn, and a stateless `openai.responses` replay turn with
     text and calls.
   - Both are sent as `output_text` and `function_call` items only.
   - A replay-only turn refuses with `protocol_incompatible`.
4. **The generated message is in place.** It is an `assistant` item directly after the first user
   item. Generated data as the last message is refused.
5. **The seed leads.** A seeded session's first and later requests carry the seed in `instructions`
   after the notice. A session with no seed carries none.
6. **Images are admitted on Responses.**
   - An image user turn sends `input_image` parts.
   - An image tool result sends its label output, then one trailing `user` item after the group.
   - The estimate counts the images and charges their dimensions.
   - The same session on exact OpenAI chat refuses.
7. **Argument limits hold.**
   - A `context_manage` call one byte over 16 KiB is cut before parsing.
   - A call whose name arrives only at `output_item.done` is still cut.
   - A `done` event carrying the full arguments does not restore an oversized call.
   - An ordinary stream with the same frames decodes unchanged.
8. **Reload prepares the same body.** A reloaded session prepares an identical digest.
9. **Ordinary Responses bytes are unchanged.** A legacy request on both plans encodes identically
   before and after the change.
10. **The refusals stay.** Each of the following is Unsupported from `managed_support` itself — so
    activation refuses, rather than activating and failing every step — and `prepare` refuses it
    before the listener accepts anything:
    - stateless mode;
    - the subscription endpoint under exact Responses, where `sends_output_ceiling()` is false;
    - the subscription endpoint with an automatic preference and a chat fallback;
    - an id not offered;
    - a non-official base URL.
11. **Limits are per model.** `capabilities` returns the same window and ceilings for each offered
    id on chat, on the API Responses endpoint and on the subscription endpoint. Image support stays
    Unknown on chat, and a proxy still declares nothing.

**Domain (`crates/nanus-domain`)**

12. **Version 6.**
    - A session whose mode record follows the model is written at version 6, with
      `"managed":true`, and read back.
    - A follows-field in a version-5 body is refused on write and on read, with the value `true`
      and with the value `false`.
    - A version-7 header is refused.
    - A session with an explicit reservation stays at its lower version.
    - A session managed once, switched to legacy, compacted, and then given a following record is
      written at version 6, and its compaction marker still validates on read. The same session's
      marker digest equals the one computed before the version-6 record was appended.
13. **The policy.**
    - `effective_reservation` returns the number when explicit, whatever its arguments.
    - When following, it returns the ceiling under `ModelWindow`, `min(ceiling, fit / 4)` under
      `Configured(fit)` (DeepSeek at 64,000: 16,000; at 200,000: 50,000; at 2,000,000 with
      `mercury-2`: 50,000), and `None` with no ceiling.
    - `same_settings` holds for two following policies with different snapshots, and fails for two
      explicit policies with different numbers.
    - A record written before the change deserialises as explicit.
    - The Rust parser refuses `"output_reserve_follows_model": false` and a non-boolean, and the
      schema test accepts the field when true and refuses it when false.

**Runner (new `crates/nanus-bundle/tests/managed_responses_wire.rs`: the real `OpenAiLlm`, through
`AgentRunner` built `with_model_context_budget`, against a local TCP listener, in the style of
`live_wire.rs`)**

14. **A managed turn on each plan.**
    - The turn checkpoints, and records an attempt whose selection protocol is `openai.responses`.
    - The received body carries `max_output_tokens: 128000` on the API plan and none on the
      subscription plan.
15. **A handoff draft on the subscription** records its attempt, and its received body has no
    `max_output_tokens`. The handoff document's policy line reads `output_reserve=model`.
16. **The reservation follows a switch.**
    - A following session moves from DeepSeek (393,216) to OpenAI (128,000) and keeps working.
    - Each request's reservation and allowance match the model in force.
    - The same switch under an explicit 200,000 makes the session unready (`unsupported_mode`),
      and the selection is not reverted.
17. **Plan switches.**
    - Switching a managed session onto the subscription endpoint under exact Responses makes it
      unready (`protocol_incompatible`, from `managed_support`), and the selection is not reverted.
    - Switching between the API and subscription plans changes the selection's endpoint digest,
      and the next attempt records it.
    - A staged proposal is revalidated by the step transaction, as for any selection change.

**Runner, budgets and recording (`crates/nanus-bundle/tests/managed_context.rs`, scripted model)**

18. **A configured budget bounds a following reservation.**
    - A runner with a configured 64,000 budget (no `with_model_context_budget`) on a model declaring
      a 393,216 ceiling and a 1,048,576 window reserves 16,000, admits a 40,000-token candidate,
      and reports ready. A 50,000-token candidate is refused with `candidate_too_large`.
    - An explicit 4,096 reservation on the same runner is used as recorded; the budget rule never
      touches an explicit number.
    - A configured budget above the window behaves as `ModelWindow` and reserves the ceiling.
19. **Recording resolves and is idempotent.**
    - Enabling with the default policy records `output_reserve_tokens` equal to the resolved
      reservation, not 4,096.
    - Enabling a following policy on a model that declares no ceiling is refused with
      `unsupported_mode` and writes nothing.
    - Recording the same following policy again after a model switch writes no record.
20. **Status reports the effective reservation.**
    - A managed, following session reports the resolved number; after a switch, the new model's.
    - A legacy session with no recorded policy reports what enabling would reserve.
    - A following session on a model with no ceiling reports the recorded snapshot,
      `managed_ready: false` and `unsupported_mode`.
21. **Exact chat now has the model's limits (legacy).** A library host's runner built
    `with_model_context_budget` on an exact-chat OpenAI adapter fits its legacy prompt to 691,500
    tokens instead of 64,000, sends `max_completion_tokens: 128000` as before, and still refuses an
    image conversation. The same runner without `with_model_context_budget` keeps 64,000.

**Clients**

22. **CLI.**
    - `--context-output-reserve model` and `--context-output-reserve 8000` parse.
    - `0`, `-1` and `models` are refused.
    - `--context-output-reserve model` on a new session without `--context-mode managed` is
      refused, as a number is.
    - A resumed explicit session keeps its number when the flag is absent;
      `--context-output-reserve model` on it records a following policy at version 6.
23. **Link.** Enabling a session with no recorded policy from the interface records a following
    policy. Enabling one whose last record is explicit keeps the number.
24. **Interface.** The activation warning no longer names Responses. Replay of a following mode
    record draws "output reserve model".

---

## Merge plan

This repository keeps `docs/` as the canonical description and lands docs with the code, and
implemented change specs stay in `.specs/changes/` with their status updated.

1. Land the Proposed changes blocks in `docs/`, the schema and `AGENTS.md` in the same change as the
   code.
2. Leave the test-count prose (`README.md`, `docs/testing.md`, `AGENTS.md`) unless asked, per
   `AGENTS.md`.
3. Set this file's Status to `Implemented`, with the date and any deviations as built, and update
   its line in `.specs/README.md`.

---

## Assumptions and open questions

**Assumptions**

- The Responses API accepts `function_call` items with no item id and no preceding reasoning item
  when `store` is false. The ordinary Responses path already sends this shape on both plans.
- The backend accepts an `assistant` item of `output_text` directly followed by another `assistant`
  item, on both plans. No live managed request has been sent on any provider.
- Every declared output ceiling is below its model's window, so a following reservation fitted to
  the window leaves a positive allowance. The smallest margin is `mercury-2`'s 78,000 tokens. Under a
  smaller budget the quarter rule leaves three quarters of it, whatever the ceilings are.

**Decisions**

- *Limits per model.* **A model's window, input ceiling and output ceiling are the same on every plan
  and wire that serves it.** This is the owner's decision (2026-10-21): the budget is a property of
  the model, not of how access is paid for. The subscription is fitted to the declared window, and
  no per-plan budget override is added.
- *Default reservation.* **The model's declared output ceiling, following the model.** This is the
  owner's decision (2026-10-21). It sends the same ceiling an ordinary request sends, so managed
  context no longer truncates reasoning models sooner than legacy context does. Following, instead
  of resolving once, lets a session switch between models whose ceilings differ by a factor of
  eight without becoming unready. The price is a smaller input allowance, which is still at least
  78,000 tokens on every supported model when the budget is the window.
- *A following reservation under a smaller budget.* **The smaller of the ceiling and a quarter of
  the budget.** This is the owner's decision (2026-10-21), approving the recommended option.
  A configured or library-default budget smaller than the window would otherwise leave no input
  allowance — DeepSeek's 393,216 against 64,000 — while status reported the session ready. The
  quarter rule is the one stock already applies to image requests (`compose.rs:1050-1066`), it is
  fixed by configuration rather than chosen per request, and it keeps following meaningful across a
  switch. Refusing activation instead would have made every host that sets a budget also set an
  explicit reserve. The 16,000 cap the image rule adds is not copied: it exists because that
  reservation is resolved once at composition, and this one is resolved per request.
- *What `Default` holds.* **A following policy with the 4,096 placeholder.** `validate` refuses 0,
  and a policy is constructed before any model is known. The placeholder is never a reservation in
  force: `change_policy` replaces it with the resolved number before recording, and the runner
  resolves per request.
- *Policy equality.* **`same_settings`, not `==`, decides a no-op.** A following policy's number is
  a snapshot; comparing it would write a record on every re-enable after a model switch.
- *Refusing `false`.* **The parser accepts only an absent field or `true`.** The writer never emits
  `false`, and an older build cannot open a body that carries the key, so a present `false` is a
  body this build did not write.
- *Recording "follows".* **An explicit field and body version 6.** A sentinel number would be
  misread by an older build as a real reservation. An unknown field fails part-way through the
  body in a build that predates it. The version rule exists to make that refusal happen at the
  header.
- *Existing sessions.* **Keep their explicit reservation, with no migration and no interface
  control.** The log is immutable and "absent is not default". `--context-output-reserve model`
  opts a session in. The owner ruled older sessions out of scope (2026-10-21), so the interface
  gains no reserve control.
- *History on Responses.* **Plain only.** Encrypted reasoning is bound to an unedited prefix, and the
  notice changes on every request, so replay would almost never be sendable.
- *Stateless replay mode.* **Stays refused.** Admitting it would mean dropping the opaque items the
  mode exists to keep.
- *Subscription reservation.* **Accounting only.** This is the owner's decision. Under the default
  it equals the model's ceiling, so nothing is lost by not sending it.
- *Subscription configurations that would send a ceiling.* **Refused.** An exact preference promises
  that a sent ceiling is honoured (`AGENTS.md`: "Exact endpoint output ceilings cannot be silently
  dropped"), and the backend refuses the field.
- *Images.* **In scope.** Responses carries declared image support for every offered id. Managed
  preparation already admits images wherever support is declared, and refusing them here alone
  would be a special case.
- *One prepared type.* **`PreparedCall` carries its wire.** One validation sequence keeps the
  grammar, reservation and fit checks from drifting between wires.
- *Protocol label.* **`openai.responses`.** This matches the existing wire-name labels. A selection
  identity and a replay envelope are different fields, and nothing compares them.
- *Status frame.* **Reports the effective number and gains no field.** A client needs the number
  in force, and the recorded policy is readable from the session. When there is no effective number
  the recorded snapshot is reported with the session unready, so the field is never empty.
- *Compaction digests.* **Stay at version 5.** A digest names a prefix under a fixed header; tying it
  to the body's current version would invalidate a marker whenever a later record raised it.
- *Exact chat's legacy budget.* **Moves with A5.** A library host that asked for the model's budget
  gets it on chat too; one that did not is unaffected.
- *Benchmarks.* **Pin an explicit 4,096 policy.** The managed benchmarks measure the hot path, not
  the default, and a pinned policy keeps saved baselines comparable.

**Open questions**

(None at this stage.)

---

## Deviations as built

- **Tests pinned to an explicit reservation.** The fixtures the spec names (the domain's
  version-specific tests, the bundle harness `managed()`, the link's seeded policy, and the bench
  fixtures) pin `output_reserve_follows_model: false` and 4,096.
- **Link test models declare a 4,096 ceiling.** The link's `ScriptedLlm` and `ManagedLlm` declare the
  number the old default reserved, so enabling over the link follows a model that has a ceiling.
- **Recording resolves after the compaction check.** `change_policy` refuses a compacted session with
  `compacted_session` before it resolves a following reservation.
- **Responses decoder.** The argument limits sit on the existing `responses::StreamAccumulator`
  (`set_argument_limits`), with the chat decoder's rules.
- **A test-only transport override.** The runner tests serve the OpenAI wire from a local listener
  through the adapter's `test-endpoint` feature, a dev-dependency of `nanus-bundle` only. It changes
  where the transport sends and nothing else: every support decision still reads the configured
  endpoint. The runner tests wrap the real adapter to declare a 393,216 ceiling and a 1,048,576 window,
  so the spec's DeepSeek-to-OpenAI numbers are the ones tested.
- **Attempt counting in the runner tests.** Attempts are counted once per `attempt_id`, since each writes a
  start and a settle record.

## Benchmark

`wire/stream` was run on the parent revision (`--save-baseline before`, with the new `openai_responses`
case added to the file in both trees) and on the change (`--baseline before`), twice, on the same
machine. The Responses stream's mean is 6.87 ms on the baseline, 6.62 ms in the first run (2.7% faster, p = 0.07,
no significant change) and 6.67 ms in the second (1.8% faster, p = 0.04). DeepSeek, OpenAI chat and
Anthropic show no significant change in the second run; the first run's +11% DeepSeek verdict did not
repeat. Two allocation counters differed in the full-suite comparison. Measured alone, on the parent and the change,
the Anthropic case shows no difference (30,364.42 on the parent, 30,364.14 on the change; the Anthropic
adapter is unchanged, and reverting the domain, tui, link, bundle and cli crates in a scratch workspace
gives 30,364.34). The earlier +11 was against a baseline recorded in the full run and is not reproduced.
The Responses case is 20,272.07 on the parent and 20,272.48 on the change, measured alone: about 0.4
allocations per iteration (0.002%). Swapping one file at a time into the parent workspace does not add up
to that: the changed `config.rs` alone reads 20,269.5 (2.5 fewer), the changed `responses.rs` alone reads
20,272.22 (0.15 more), and the whole change reads 20,272.48. No single file's change is the cause, and the
ordinary decode path's code is unchanged in what it does, so I read the 0.4 as an effect of how the binary
is built rather than of the code's behaviour.

Narrowed by measurement. A decoder-only benchmark (`wire/decode`, the Responses accumulator fed the same
frames with no socket) allocates exactly 2,018.0 per iteration on the parent and on the change, with
zero-width intervals. The decoder's own allocation count is therefore unchanged. The stream residual sits
in the dispatch and transport path around it.

Accepted deviation (owner decision, 2026-10-22). The requirement that an ordinary stream "should not move"
is met to within 0.002%, not exactly. The full Responses stream allocates 20,272.48 per iteration on the change
against 20,272.07 on the parent, a difference Criterion reports as statistically significant (p = 0.03). The
decoder-only count is identical on both trees (2,018.0), and the decode and encode code is unchanged in what it
allocates. The residual sits in the dispatch and transport path. Swapping single files into the parent moves the
count by up to 2.7 allocations (20,269.5 to 20,272.2) without adding up to the change, so it is within the
build-to-build variance of this benchmark rather than a source I could attribute. The owner accepts it as a
documented deviation.

The ordinary Responses bytes are pinned by `crates/nanus-adapter-openai/tests/ordinary_responses_bytes.rs`,
whose goldens were taken from the parent revision, so the shared encoder is unchanged for ordinary requests.

## Coverage

Every acceptance case has a passing test. Where a test checks a narrower surface than the case's
wording, the case says so here:

- Case 17, staged proposals: `proposal::recheck` refuses a profile that changed, tested directly in
  `crates/nanus-domain/tests/staged_proposal_revalidation.rs`. At the step level,
  `a_selection_change_while_a_proposal_is_staged_lands_after_the_step_settles` in
  `managed_context.rs` stages a proposal in a step whose model answer asks for a different model.
  The proposal is accepted under the profile its step was prepared with, the step settles with its
  revision, and the switch lands only after the settle. The selection hold makes that ordering a
  property of the step, not of the test's timing.
- Case 22, end to end: `crates/nanus-cli/tests/managed_reserve_cli.rs` runs the real `nanus` binary with
  `nanus run --resume <id> --context-mode managed --context-output-reserve model`. The stored session is
  written at version 6 with a following policy, and the turn's request carries `max_output_tokens` of
  128,000 to a local listener. This needs the `test-endpoint` feature, which only the workspace test run
  enables. The feature points the OpenAI transport at a listener named by `NANUS_TEST_OPENAI_ORIGIN`; the
  support decisions still read the official endpoint, so the managed path is the production one. The flag
  parser and its mapping are also tested in `cli.rs`.
- Case 21: the legacy prompt budget is a public accessor (`AgentRunner::prompt_budget`), asserted at exactly
  691,500 for a fitted exact-chat runner and 64,000 for the configured one. On the wire, a legacy conversation
  of about 100,000 tokens is sent whole by the fitted runner and has its oldest turn elided under 64,000
  (`managed_responses_wire.rs`).
- Cases 1, 2, 7, 8: `crates/nanus-adapter-openai/src/managed_tests.rs` (listener-level bytes, reload,
  argument limits, the subscription body).
- Case 3: plain history, the Anthropic and stateless replay turns sent as text and calls, and the
  replay-only refusal.
- Case 4: the generated message placed after the first user item, and refused last.
- Case 5: the seed joins `instructions` after the notice, and none without a seed.
- Case 6: images on Responses, refused on exact chat, and an image tool result answered with its label,
  its pixels following as one trailing user item.
- Case 9: the ordinary Responses bytes, pinned by `tests/ordinary_responses_bytes.rs`.
- Case 12: a compacted session that gains a following record is version 6, and its marker still
  validates (`crates/nanus-domain/tests/reserve_follows_model.rs`).
- Cases 14 to 16: `crates/nanus-bundle/tests/managed_responses_wire.rs`, including the handoff on the
  subscription and the explicit-200,000 switch.
- Case 17, plan switches: the endpoint digest changes with the plan, and an exact subscription is
  unready with `protocol_incompatible`.
- Cases 18, 19: the configured budget and recording, in `managed_context.rs`.
- Case 20: status across a switch, for a legacy session, and on a model with no ceiling.
- Case 21, the legacy request: `max_completion_tokens` of 128,000 on exact chat (`managed_responses_wire.rs`).
- Case 23: the link records a following policy on enable and keeps an explicit one.
- Case 24: the activation warning no longer names Responses, and the replay line says `model`.

The benchmark is recorded above. The Responses stream is 1.8% faster on the second run, and no other
stream benchmark changed significantly.
