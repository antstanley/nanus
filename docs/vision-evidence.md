# Vision evidence and embedding verification

The implementation is available. Eight exact models are promoted to Supported on live evidence
from 2026-10-01 (below); every other model, plan and wire stays Unknown and refuses pixels. Native
Windows and macOS execution also pass. The
[proposal](../.specs/changes/2026-09-30-library_embedding_and_multimodal_results.md) remains
Proposed until its acceptance gates pass. CI added here verifies the portable library,
not the Unix CLI/link/service.

| Exact model / protocol | Candidate profile | Captured bytes/order + v2 store reload | Live image answer + call-reference follow-up |
|---|---|---|---|
| `claude-opus-5-5` / Messages | `anthropic-opus55-high-patch28-v1` | Fixture test | Passed live 2026-10-01 (PNG, JPEG, WebP, GIF); Supported |
| `claude-sonnet-5-5` / Messages | `anthropic-sonnet55-high-patch28-v1` | Independent fixture test | Passed live 2026-10-01 (PNG, JPEG, WebP, GIF); Supported |
| `gpt-6-astra` / Responses (`api.openai.com`) | `openai-astra-high-patch32-v1` | Fixture test | Passed live 2026-10-01 (PNG, JPEG, WebP, GIF); Supported |
| `gpt-6.1-sol` / Responses (`api.openai.com`) | `openai-sol61-high-patch32-v1` | Shared OpenAI encoder test | Passed live 2026-10-01 (PNG, JPEG, WebP, GIF); Supported |
| `gpt-6-luna` / Responses (`api.openai.com`) | `openai-luna6-high-patch32-v1` | Shared OpenAI encoder test | Passed live 2026-10-01 (PNG, JPEG, WebP, GIF); Supported |
| `gpt-5.6-sol` / Responses (`api.openai.com`) | `openai-sol56-high-patch32-v1` | Shared OpenAI encoder test | Passed live 2026-10-01 (PNG, JPEG, WebP, GIF); Supported |
| `gpt-5.6-terra` / Responses (`api.openai.com`) | `openai-terra56-high-patch32-v1` | Shared OpenAI encoder test | Passed live 2026-10-01 (PNG, JPEG, WebP, GIF); Supported |
| the six OpenAI models above / Responses (`chatgpt.com/backend-api/codex`, `subscription` plan) | their profiles above | Shared OpenAI encoder test | Each passed live 2026-10-01 on this endpoint (PNG, JPEG, WebP, GIF); Supported |
| `gpt-5.6-luna` / Responses (`api.openai.com`) | `openai-luna56-high-patch32-v1` | Shared OpenAI encoder test | Passed live 2026-10-01 (PNG, JPEG, WebP, GIF); Supported |

The fictional fixture is a green triangle on white, supplied as PNG and JPEG. Tests compare
complete decoded file bytes, original text order, IDs and error flags after actual store reload.
The fixture sources live in each adapter's `tests/multimodal.rs`; image files live in
`nanus-domain/tests/data`. These are local encoding proofs, not accepted provider transcripts.
Before promotion, record the immutable tested revision, profile version, canonical request
fixture digest, contract retrieval date, exact live model/protocol, and both objective answers.
A profile or contract change invalidates prior evidence. Credentials and authorization headers
must never enter evidence artifacts. An HTTP success by itself is insufficient.

Contracts retrieved on 2026-10-01:
[Anthropic vision](https://platform.claude.com/docs/en/build-with-claude/vision),
[model catalogue](https://platform.claude.com/docs/en/models/overview),
[preserved thinking](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking),
[OpenAI vision](https://developers.openai.com/api/docs/guides/images-vision), and
[GPT-6 Astra](https://developers.openai.com/api/docs/models/gpt-6-astra).

Anthropic candidate images have a 2576-pixel long edge and at most 4784 patches of 28×28;
1920×1080 produces 2691 patches and 2576×1456 produces 4784. OpenAI's candidate bounds
both edges to 1024 with high detail and charges `ceil(6 × patches32 / 5)`. Each profile
reserves `ceil(5 × (base + 32) / 4)` tokens, a library safety policy rather than billed usage.
PNG/JPEG/WebP and still GIF (an animated GIF is refused), 512 KiB per file, four images/32 blocks per result, eight images/4 MiB per
request; no automatic resize or crop. Text/framing/schemas are conservatively charged at
one serialized byte per token after translation. Base64 contributes to bytes, not visual cost.

`with_request_budget(output, separate_reasoning)` names the actual output ceiling and any
reasoning outside that ceiling. Hosts that include reasoning in output use zero separately;
Hype Studio should reserve at most 8192 total. The metadata and caller ceiling are checked
before HTTP. Unknown capabilities never infer image support from an alias, vendor or plan.
Responses returns `unsupported-image-protocol`; other profile-less combinations refuse pixels.

Native Windows and macOS passed both the minimal and explicit-provider downstream
configurations on implementation revision `890888f8519e988dbd19505d7b63c60e11b9ecfb`.
The [native feature-matrix run](https://github.com/antstanley/nanus/actions/runs/36793542070)
completed successfully on 2026-10-01; every job tested the locked fixture and ran the
caller-owned host. This proves the supported library subset, not the Unix CLI/link/service.
## Live evidence, 2026-10-01

`crates/nanus-bundle/tests/live_vision.rs` (`#[ignore]`d; it spends credit) sends the body each
adapter's own `encode` produces for a two-call session: call `alpha` returns the fixture image,
call `beta` returns text. Each model must describe a green triangle, then name `alpha` as the call
that returned the image. A call id is not used because Anthropic never shows it to the model. All
combinations (eight models × PNG, JPEG, WebP, GIF) passed, non-streamed and without tools in the
request. The real agent then ran `nanus run` with `read_image` end to end on Sonnet 5.5 and
`gpt-6-astra`: the tool, the image request and the answer all completed. Every run on this page was
made on the working tree that became commit `479f6de` (parent `b1a1854`); only documentation and
test-count text changed between those runs and the commit.

`gpt-6-astra` is sent to the Responses API (chat completions refuses function tools beside a
reasoning effort), with pixels as `input_image` items after the group of tool results. The
`ChatGPT` subscription backend is a different endpoint and was run separately, model by model
(streamed, as that backend requires, with no output ceiling in the body): all six OpenAI models
passed there, and the real agent ran `read_image` end to end on `gpt-6.1-sol` over it. Fable 5.1
and Haiku 4.5 have no evidence and stay Unknown. A profile belongs to one exact model id, so a later model (`gpt-6.2-sol`,
say) is Unknown until it has a profile and its own live run.

## `read_video`

`read_video` ([the tool](features.md#read_video-optional)) was run through the real agent on
2026-10-02, over a generated eight-second clip: four colours (red, green, blue, yellow), two seconds
each, carrying one to four white squares. The expected answer is exact — 1040 ms red with one square,
3040 ms green with two, 5040 ms blue with three, 7040 ms yellow with four — and a run counts only when
the model reproduces the colours in order. The agent was `nanus run --approval permitted` with
`read_video = true`, the credentials its own store holds, and an isolated `NANUS_HOME`.

| Conversation model | Plan | Delivery `auto` chose | Result |
|---|---|---|---|
| `claude-opus-5-5`, `claude-sonnet-5-5` | api | frames (pixels to the model) | correct |
| `claude-fable-5-1`, `claude-haiku-4-5-20251001`, `claude-sonnet-5`, `claude-opus-5` | api | analysis by `claude-sonnet-5-5` | correct |
| `gpt-6-astra`, `gpt-6.1-sol`, `gpt-6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna` | api | frames | correct |
| the same six | subscription | frames | correct |
| `claude-sonnet-5-5`, `gpt-6-luna` with `mode: analyze` | api, and subscription for `gpt-6-luna` | analysis by the same model | correct |
| `claude-haiku-4-5-20251001` with `mode: frames` | api | refused before the file was opened; the model retried `auto` | correct after the retry |

That is 23 runs, every one reproducing the colours and timestamps. Frames are sent to the main
model only where it has the profile above; for every other Anthropic model the analysis request goes
to `claude-sonnet-5-5` on the same account, and the manifest names it. Each of the fourteen OpenAI runs failed its first
call, because OpenAI models fill every optional argument and sent `end_ms: 60000` for an eight-second
clip. The tool now clamps an overshooting `end_ms` to the end of the video and says so in the manifest's
warnings, and nine representative runs from `scripts/live-read-video.sh` afterwards had no failed call.
The persisted manifest on the subscription endpoint carries provider, plan, model, endpoint origin
(`https://chatgpt.com`, no path), protocol, profile version and the usage the backend reported.

Admission was exercised live too: asked for two parallel four-frame reads in one step, `claude-sonnet-5-5` got
the first and was refused the second *before it ran* ("this turn already holds 4 image(s) and 2796208 encoded
bytes, this call may add 4 and up to 2796208 bytes, and a request holds at most 8 images and 4194304 bytes"),
and reported the refusal accurately. The same run before the tool declared its envelope let both calls through,
which is how an edit that silently did not apply was caught.

Not covered by this evidence: DeepSeek and z.ai (no model with live image evidence, so no analysis
route), and native Windows and Linux (see [status](status.md#known-limits)). A later model has no analysis route until it has a profile
and its own live run, exactly as above. Reproduce with `scripts/live-read-video.sh`, which spends credit.
