# Vision evidence and embedding verification

The implementation is available; none of the built-in image capabilities is promoted to
Supported. Live model acceptance remains pending; native Windows and macOS execution now pass. The
[proposal](../.specs/changes/2026-09-30-library_embedding_and_multimodal_results.md) remains
Proposed until its acceptance gates pass. CI added here verifies the portable library,
not the Unix CLI/link/service.

| Exact model / protocol | Candidate profile | Captured bytes/order + v2 store reload | Live image answer + call-reference follow-up |
|---|---|---|---|
| `claude-opus-5-5` / Messages | `anthropic-opus55-high-patch28-v1` | Fixture test | Pending; Unknown |
| `claude-sonnet-5-5` / Messages | `anthropic-sonnet55-high-patch28-v1` | Independent fixture test | Pending; Unknown |
| `gpt-6-astra` / Chat Completions | `openai-astra-high-patch32-v1` | Fixture test | Pending; Unknown |

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
PNG/JPEG only, 512 KiB per file, four images/32 blocks per result, eight images/4 MiB per
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
Exact-model live vision follow-ups still have no available credentials and are not counted
as passed. The proposal remains Proposed and all built-in image capabilities remain Unknown.
