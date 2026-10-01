# Video-input provider research

**Status:** Draft · **Date:** 2026-10-01 · **Owner:** Ant Stanley · **Scope:** read_video desk research

This is documentation research for the [optional read_video design](../changes/2026-10-01-read_video_extension.md).
It covers only Nanus's four installed providers: DeepSeek, z.ai, Anthropic and OpenAI.
Additional providers will be evaluated after their harness integration ships.
No paid request, decoder experiment or latency/quality benchmark was run. Documentation
support, model availability on a plan, and Nanus's verified wire/replay support are separate
claims. Recheck contracts before implementing; model labels do not establish capability.
The revised tool is repo-maintained and samples JPEGs with FFmpeg for every route. Native
movie input is background research, not an implementation prerequisite. All current main
models must work through direct images or same-provider sampled-frame analysis.

## Provider and model matrix

“Native video” means the API accepts a video asset/content part. Providers can still sample
frames internally. “Frames” means our extension extracts stills and submits images; that
provides sampled visual evidence, not native video or audio understanding.

| Provider | Native video input | Models relevant to video inspection | Nanus today |
|---|---|---|---|
| z.ai API | Documented. | `glm-5.3-flash`, `glm-5.3-flashx`; `glm-4.6v`, `glm-4.6v-flashx`, `glm-4.6v-flash`; `glm-4.5v`. | The two 5.3 Flash IDs are already offered; no z.ai image or video profile is promoted. The 4.x V models are possible extension backends, not current stock model entries. |
| z.ai Coding Plan | Video analysis is documented through its separate Vision MCP server. Direct video wire/plan combinations need qualification. | `glm-5.3-flash` is on the plan; the guide says `glm-5.3-flashx` is not yet available there. | Stock coding-plan selection is not proof of native-video entitlement. MCP is a separate adapter option. |
| OpenAI API | The six offered models explicitly list video as unsupported. | Frame candidates: `gpt-6-astra`, `gpt-6.1-sol`, `gpt-6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`. | These exact IDs have verified image profiles on Responses; extracted frames fit the existing image path. No native video content. |
| OpenAI subscription | No native-video evidence for this backend. | Same six frame candidates. | Independent image evidence exists for all six on `chatgpt.com/backend-api/codex`; public API capability/credentials cannot be assumed equivalent. |
| Anthropic API | No documented native-video part found in the inspected Messages/vision contracts; treat as unqualified. | Frame candidates among offered models: `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-fable-5-1`, `claude-haiku-4-5-20251001`, `claude-opus-5`, `claude-sonnet-5`. | Only Opus/Sonnet 5.5 have promoted image profiles. Other offered models must remain Unknown until separately qualified. |
| DeepSeek API | No documented native-video path found in the inspected image contracts. | `deepseek-flash` now documents image input. `deepseek-v4-pro` is listed without vision. | Both IDs are offered; neither has a promoted Nanus image profile. Flash is a future frame candidate, not ready in this checkout. |

The current offered-model inventory comes from [Features → Model providers](../../docs/features.md#model-providers).
The exact image promotion list comes from [vision evidence](../../docs/vision-evidence.md).
The external support evidence for each row is linked below. No native video profile is
implemented or qualified by this research. z.ai's text `glm-5.3` and `glm-5.2` must not acquire
video support because their names resemble the Flash/V variants.

## z.ai: native input and a separate coding-plan tool

The [GLM-5.3 Flash/FlashX guide](https://docs.z.ai/guides/vlm/glm-5.3-flash)
explicitly advertises images, videos and files. It names `glm-5.3-flash` and
`glm-5.3-flashx`, with a 1M context, and distinguishes API from coding-plan availability:
Flash is available on the coding plan; FlashX is not yet there. This matters because Nanus
currently offers FlashX as a coding-plan model too. Record this as a research discrepancy;
this design-only change does not edit unrelated model-selection behavior.

The [GLM-4.6V family guide](https://docs.z.ai/guides/vlm/glm-4.6v)
lists video/image/text/file input for the main, FlashX and Flash variants, with 128K context.
The [GLM-4.5V guide](https://docs.z.ai/guides/vlm/glm-4.5v) also documents video input.
These are alternatives if the newer Flash wire cannot yet be pinned reliably.

The [chat completion reference](https://docs.z.ai/api-reference/llm/chat-completion)
advertises multimodal video input, but its retrieved rendered message union did not expose
a complete `video_url` contract. Do not invent an accepted block, base64/file-ID mapping,
audio behavior, byte cap or timestamp convention. Capture the chosen exact endpoint/model
request and response before implementation acceptance. Advertised modality is enough to
select a candidate, not to promote a working extension profile.

The official [Vision MCP server guide](https://docs.z.ai/devpack/mcp/vision-mcp-server)
documents a local Coding Plan tool bridge and `video_analysis`, accepting local/remote
MP4/MOV/M4V up to 8 MB. It currently points to the GLM-5.3 Flash capability and requires
Node.js ≥22. Treat its 8 MB cap as specific to this bridge, not the direct model API.
This bridge is background research, not the selected implementation: the repo-owned tool
uses FFmpeg and existing image adapters, so bridge-specific source formats, 8 MB limits and
Node.js are not its dependencies. Remote URLs remain outside the local-file contract.
This research did not install or invoke the MCP server.

## OpenAI: images for frame inspection

The modality sections for [GPT-6 Astra](https://developers.openai.com/api/docs/models/gpt-6-astra),
[GPT-6.1 Sol](https://developers.openai.com/api/docs/models/gpt-6.1-sol),
[GPT-6 Luna](https://developers.openai.com/api/docs/models/gpt-6-luna),
[GPT-5.6 Sol](https://developers.openai.com/api/docs/models/gpt-5.6-sol),
[GPT-5.6 Terra](https://developers.openai.com/api/docs/models/gpt-5.6-terra), and
[GPT-5.6 Luna](https://developers.openai.com/api/docs/models/gpt-5.6-luna)
all list image input and mark video unsupported. The
[image guide](https://developers.openai.com/api/docs/guides/images-vision) supplies the
image-input contract. Returning timestamped JPEGs uses an already tested Nanus wire;
it does not enable a video modality. Audio would require a separate transcription or
analysis path. The agent should never claim to have heard the recording from images.

A video generation API/model is not evidence for understanding an existing movie.
Product UI camera/screen sharing also does not prove support on Nanus's API or subscription
endpoint. The proposed extension makes no native OpenAI video-support claim.

## Anthropic: image sequences, not a proven movie upload

The [model overview](https://platform.claude.com/docs/en/models/overview) documents text
and image input for current Claude models. The
[vision guide](https://platform.claude.com/docs/en/build-with-claude/vision) documents multiple
labelled images and supports JPEG/PNG/GIF/WebP; an animation is treated as its first frame.
The inspected [Messages API](https://platform.claude.com/docs/en/api/messages/create)
does not establish a native video part. The conclusion is **no documented native path
found in these contracts**, not a claim about every Anthropic product or future release.

Use timestamped extracted frames on the two already verified 5.5 profiles. Larger provider
image allowances do not override Nanus's four-image result/eight-image request limits.
Repeated image history still costs tokens, so a small window and explicit sampling matter.

## DeepSeek: newer image support; no qualified video path

The [current quick start](https://api-docs.deepseek.com/quick_start) names `deepseek-flash`
and `deepseek-v4-pro`. Its [vision guide](https://api-docs.deepseek.com/guides/vision/)
documents Flash image input using Chat Completions, Anthropic-compatible Messages and
Responses shapes. The [pricing/capability table](https://api-docs.deepseek.com/quick_start/pricing/)
marks Flash vision supported and Pro unsupported. This corrects the assumption that all
DeepSeek API models are text-only.

The [September 10 release note](https://api-docs.deepseek.com/news/news260910/)
identifies the current Flash service as V4.1 with multimodal support. Old V4-Flash/Vision-Exp
names are compatibility routes, not additional models to add to our spec. This does not
establish a movie-input contract. Nanus's existing DeepSeek adapter still needs an exact
image profile, encoding fixtures and live follow-up before frame mode can use it.

Retrieval limitation: the English vision guide was available as a detailed official search
extract but repeatedly failed direct page opening. The quick start opened successfully;
capability/table and release evidence were independently discoverable. Re-fetch the full
vision/request schema before coding, especially role/tool-result placement and media limits.

## FFmpeg sampling and codec coverage

The tool accepts local formats/codecs enabled in its certified FFmpeg build and normalizes
all selected video frames to JPEG before provider requests. WebM support is a demuxing
requirement; AV1 is a separate decoding requirement, including in MP4/Matroska. Codec
coverage is independent of main-model modalities and provider movie-upload restrictions.

The [formats reference](https://ffmpeg.org/ffmpeg-formats.html) explains that demuxers can
be disabled at build time. The [codec reference](https://ffmpeg.org/ffmpeg-codecs.html)
documents AV1 decoding, including native `av1` and optional `libdav1d`; the latter requires
build configuration. Therefore “FFmpeg installed” alone does not prove required coverage.
The [CLI reference](https://ffmpeg.org/ffmpeg.html) supplies build/decoder/demuxer inventories;
[FFprobe](https://ffmpeg.org/ffprobe.html) supplies stream/container inspection. Certify the
configured executable pair with actual fixtures, including WebM and AV1; no codec tests
were executed for this documentation update.

## Compatibility route for every supported main model

Native image support is not required on the main conversation model. Default `auto` returns
sampled pixels when that model's exact profile is verified; otherwise a verified vision
model on the same provider/plan interprets the same images and returns text. The extension
keeps the main model and credential account unchanged. Configuration declares this analysis
route and cost rather than asking the model to invent a provider fallback.

| Main provider/plan | Default sampled-image analyst | Current prerequisite |
|---|---|---|
| DeepSeek API, both offered models | `deepseek-flash` | Capture/live-qualify the documented Flash image wire. |
| z.ai API or coding, all four offered models | `glm-5.3-flash` | Capture/live-qualify image profiles independently on each endpoint/account. |
| Anthropic API, all six offered models | `claude-sonnet-5-5` | Existing image profile; qualify the tool's analysis request/replay. |
| OpenAI API or subscription, all six offered models | `gpt-6-luna` | Existing profiles on both endpoints; exercise independent route/budget behavior. |

These are design choices using existing providers/models, not observed read_video results.
OpenAI subscription's current encoder omits max_output_tokens, so the tool cannot promise
2048 generated tokens on that endpoint. Its budget policy must qualify and reserve the full
backend answer/reasoning ceiling (currently advertised as 128000), independently of bounded
local answer collection. Cancellation may leave a billable/incomplete charge. Direct-frame
hosts also need the proposed shared admission ledger for retained/pending image outcomes;
existing individual image checks are not aggregate preflight.
A text-only Pro/GLM selection gets useful text rather than an image-capability refusal.
Unknown main-model image input also follows analysis without promotion. Unknown **analysis**
input is still refused. Full model coverage remains an implementation acceptance gate; it
cannot be claimed until required Flash profiles and all model/plan routing tests pass.

## Suggested bench protocol (not executed)

Evaluate `auto` across every currently supported configured model/provider/plan combination,
using the runtime catalogue rather than a hand-picked vision subset. Exercise direct JPEG
results and same-provider analysis/text results separately. Source tests must cover the
certified FFmpeg build's standard codecs, WebM with VP8/VP9/AV1, AV1 in MP4/Matroska,
H.264, HEVC, MPEG-4 Part 2, MPEG-2 and MJPEG. Use reproducible non-sensitive clips:

| Fixture | What it tests |
|---|---|
| 8-second colour/shape sequence with timestamped transitions | State order and sampled-source timestamp grounding. |
| UI recording with an error dialog and labelled controls | OCR and interaction explanation through both delivery routes. |
| Brief event deliberately placed between sparse frame samples | Omission honesty; compare denser sampling in a later explicit call. |
| Visually identical clips with different speech tracks | Sampling result omits audio and makes no acoustic claim. |
| Rotated MOV, variable-FPS MP4 and WebM/AV1 variants | Decoder timeline/geometry and required codec coverage. |
| Malformed, external-reference and oversized files | Fail-closed bounds, containment and cleanup. |

Fix question, interval, frame count, output budget, JPEG dimensions and sampling policy.
Record main-model selection, analysis-model/endpoint/plan when used, FFmpeg build/decoder
inventory, fixture/JPEG digests and actual timestamps. Run five trials per supported
combination; exercise generation beyond the answer target, stream overflow/cancellation,
missing usage and concurrent/mixed image-result admission. Report accuracy, timing error,
missed-event rate, probe/decode latency, time
to first answer, total latency, provider-reported input/output/reasoning tokens and cost
using that day's rates. Report medians/ranges rather than p95 from five samples. Account
for both main-model and analysis-model work instead of treating the helper as free.

Profile promotion needs exact captured JPEG bytes, a live sampled-temporal answer and a
persisted follow-up; every configured main-model route must also be exercised. Main models
without vision must receive only text. No provider ranking, live promotion, local codec
coverage or full-model implementation claim follows from this desk research.

## Assumptions and open questions

**Assumptions**

- The four installed provider inventory and image-evidence page match the inspected revision.
- “Native” describes the input API, not guarantees about internal frame coverage or audio.

**Decisions**

- _Implementation._ **Repo-maintained FFmpeg sampling.** One source-decoding pipeline
  supports standard enabled codecs with mandatory WebM and AV1 coverage.
- _Model compatibility._ **Every supported main model via auto delivery.** Verified main
  vision models receive pixels; all others receive same-provider vision-analysis text.
- _Native input._ **Research only.** The sampled-image tool does not need movie-input APIs.
- _Provider scope._ **Existing harness providers only.** Revisit additional providers
  after their harness integration ships.
- _Coding subscriptions._ **Qualify separately.** Same provider name does not establish
  endpoint entitlement, identical model availability, wire support or credential scope.
- _Evidence._ **Desk research only.** No live calls or benchmark results are invented.

**Open questions**

- Which certified FFmpeg distributions/builds satisfy the mandatory codec suite on each OS?
- Can DeepSeek Flash and z.ai Flash image profiles pass the required endpoint-specific wire,
  live sampled-temporal and replay gates? They are prerequisites for full model coverage.
- When should the pre-existing z.ai coding-plan FlashX catalogue discrepancy be corrected?
