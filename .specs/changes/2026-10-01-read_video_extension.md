# Change: Optional read_video extension

**Status:** Partially implemented (2026-10-02) · **Date:** 2026-10-01 · **Owner:** Ant Stanley · **Target:** Repo-maintained optional tool extension

Provide `read_video` as an optional extension maintained in this repository at
`crates/nanus-tool-video`. FFprobe will inspect local media and FFmpeg will sample timestamped
JPEGs from every input. The tool will support all currently configured models the harness
supports: verified vision models can receive those images directly; other models receive a
text interpretation from a verified vision model on the same provider and credential plan.
The main conversation model is unchanged. Stock seven-tool composition and five goal tools
keep their contracts. This is initial design, not an implementation claim.

The [provider research](../research/2026-10-01-video_provider_support.md) distinguishes API
capabilities from current Nanus evidence. Provider scope remains DeepSeek, z.ai, Anthropic
and OpenAI; additional providers are evaluated after harness support ships. WebM demuxing
and AV1 decoding are explicit requirements alongside FFmpeg's standard available codecs.

## Caller-owned snapshot seam — 2026-10-03

The local working copy adds `Snapshot::from_owned_file(path, sha256, byte_len, owner)`
and `retain_owner()`. The constructor accepts a host-verified immutable fixed-name
`source` file and an opaque `Arc<dyn Any + Send + Sync>` lease. It validates receipt
shape and the existing 128 MiB ceiling without reading, copying, probing, spawning or
discovering credentials. A valid receipt shape does not establish filesystem scope,
content digest or physical identity; the caller's rooted snapshot adapter owns those
checks and the final cleanup policy. Empty claimed length remains representable as
in the existing source, with format validity determined by the decoder.

Physical workers retain the opaque owner before leaving the local call, and release it
only after their process/readers have joined. Dropping a call/future cannot remove a
copy still held by such a worker. Stock `FsSource` now retains its existing TempDir in
the same opaque owner; copy/hash/bounds and relative temp-root behavior are unchanged.
Snapshot Debug omits the owner. The public source trait can therefore be implemented
directly by a host without enlarging its ordinary authoring-file port or invoking stock
composition. This is a generic media lifetime seam, with no app workflow, skill or plugin.

Four external-caller fixtures cover actual image/manifest delivery, retained physical
worker ownership and final cleanup, rejected receipt transfer, and I/O-free inclusive
receipt bounds. Eleven library/snapshot tests and ten existing real FFmpeg regressions
pass; workspace warning-denying Clippy, formatting and Windows-target library Clippy
pass. The package doctest command passes with zero examples. The
[semi-formal certificate](2026-10-03-video_snapshot.review.md) records initial fixture/lint
corrections and scope. This seam is unpublished and Hype has not adopted or registered
the extension. Native Windows execution and original video/provider acceptance remain open.

## Generic returned-image byte enforcement — 2026-10-03

The local unpublished [envelope fix](2026-10-03-enforce_image_envelope_bytes.md)
now enforces the reserved raw-file bytes of every image, including failure content,
before completion/progress retention. Exact equality, all base64 padding cases,
next-byte overflow and mixed-call isolation pass actual-runner tests. The published
80a0f79b pin still checks count only. This closes the narrow local returned-byte gap;
it does not implement full `ToolAdmission` request/token projections or desktop
worker/source/payment authority, and does not complete this video proposal.

## Generic complete-batch lifecycle prerequisite — 2026-10-03

The unpublished [optional admission implementation](2026-10-03-tool_batch_admission.md)
now supplies full/fitted pending request and actual durable-event projections, held selection, owned batch
reservation callbacks and raw/normalized validation before progress. It passes 419
credential-free domain/ports/minimal tests and 24 all-feature admission cases. This
implements the generic lifecycle seam only; the host still owes complete result/wire/
token/checkpoint budgets, source/worker/selection authority and analysis charges.
Published 80a0f79b and Hype's pin lack it. Original native/provider/video gates remain open.

## Implementation status, 2026-10-02

Implemented in `crates/nanus-tool-video`, `nanus-domain` (`ImageEnvelope`), `nanus-bundle` (`video.rs`, admission in
`agent_loop.rs`), installed by `read_video = true`: FFprobe/FFmpeg sampling with the mandatory codec/demuxer check
at startup, the bounded rooted source, `auto`/`frames`/`analyze`, the manifest, same-provider analysis for
**Anthropic** (`claude-sonnet-5-5`), **OpenAI** (`gpt-6-luna`, API and subscription) and **DeepSeek**
(`deepseek-flash`), an analysis budget
(reserve before sending, settle to reported usage, keep the whole reservation on failure, missing usage or
cancellation), and batch image admission. Verified live on every Anthropic, OpenAI and DeepSeek model and plan in the
catalogue, including a live refusal of an over-capacity second call; see
[the evidence](../../docs/vision-evidence.md#read_video).

Not implemented, and why the change stays open:

- **A z.ai route.** No z.ai model has an image profile with live evidence and its credential is not stored here, so it
  cannot be qualified; `analyze` and `auto` report that rather than guessing. The spec's "every configured model" claim
  is therefore **not** met for z.ai. DeepSeek was qualified on 2026-10-04: `deepseek-flash` is Supported with a measured
  profile and is the analysis model for DeepSeek, and `deepseek-v4-pro` is shown live not to read images.
- **Linux and Windows certification,** and process-lifecycle evidence beyond a dropped call killing the decoder on Unix.

Narrower than specified: admission is a generic _declared-envelope_ reservation (`with_result_images`) against the
newest turn and the model's image and byte caps, not the full `ToolAdmission` projection with selection epochs and
cross-chunk failure slots; a model switch during a step is caught by the existing next-request validation, not
by admission. The analysis budget is per agent process and is not persisted.

Deviations from the text above: an `end_ms` past the end of the video is clamped with a warning rather than
refused (live OpenAI models send `end_ms: 60000` for every clip); the analysis request carries a one-line
`sample_frames` declaration because providers refuse an image-bearing tool result without a declared tool, and
any call to it fails the analysis; the sampler is `centres-nearest-after-v2`, a coarse input seek then an exact
output seek, because MPEG program streams land an input seek late.

## Motivation

A recording can show a UI transition, reproduction steps or a failure that a single
screenshot misses. Sending an arbitrary movie into the ordinary conversation would require
new provider encoders, content types, budget estimators and durable media storage. The
existing extension/tool-result seams can support useful inspection with much less coupling.

Some configured models lack image input, and others lack a verified Nanus image profile.
The extension must provide useful visual inspection for both through sampled-frame analysis,
rather than making vision support a prerequisite for calling `read_video`. Enabling the
extension authorizes its declared analysis-model route and cost within the existing provider
account; it does not authorize a different provider, plan or credential destination.

## Affected spec pages

The canonical specification remains in `docs/` and Rust contracts; there is no global
JSON Schema to change. Apply these additions only after implementation and verification.

| Canonical page and heading                                                                                                 | Change                                                         |
| -------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------- |
| [Architecture → What each crate owns](../../docs/architecture.md#what-each-crate-owns)                                     | Add the optional extension/host boundary.                      |
| [Design → Seven tools, and the count is the design](../../docs/design.md#seven-tools-and-the-count-is-the-design)          | Add the distinction between stock and host-installed tools.    |
| [Features → The toolset](../../docs/features.md#the-toolset)                                                               | Add the installed tool's bounded contract.                     |
| [Sessions → What a session is](../../docs/sessions.md#what-a-session-is)                                                   | Add replay/provenance rules using existing text/image content. |
| [Safety → Defaults, and what they do not protect you from](../../SAFETY.md#defaults-and-what-they-do-not-protect-you-from) | Add process, disclosure and host policy rules.                 |
| [Testing → The tests that matter most](../../docs/testing.md#the-tests-that-matter-most)                                   | Add extension and video evidence gates.                        |

## Proposed changes

### Architecture → What each crate owns (Add)

> `crates/nanus-tool-video` owns the optional `read_video` extension, its FFmpeg adapter,
> sampling policy, provider-route policy and tests. Nanus maintains it in this repository
> as a separate workspace crate; it returns an ordinary `ToolDefinition`. The stock bundle
> does not depend on it. Public contracts use domain/port types; hosts inject rooted media,
> model selection, analysis adapters and budgets. An optional stock-provider integration
> wires the existing adapters without pulling them into the minimal extension API.
>
> Direct-frame delivery requires a proposed generic `ToolAdmission` host integration:
> `nanus-ports` owns its session-independent contracts; `AgentRunner` optionally supplies
> immutable request projections and outcome lifecycle callbacks; the host owns reservations.
> This is new infrastructure, not an existing ToolPolicy/ToolExecutor capability. It carries
> no video-specific dispatch or provider dependency and defaults to absent for stock hosts.
> Hosts enabling direct frames must install it and declare bounds for every offered tool.
>
> The host constructs its registry, adds the extension explicitly and shares the same
> registry handle with the runner/tools-service plugin. Kernel hosts can publish the
> extension services through ordinary plugins. Installation is fixed for a runner lifetime;
> unloading stops/drains and disposes the runner before withdrawing services. Live removal
> of a single tool is outside this contract. No FFmpeg installation, Node runtime or new
> provider is required by ordinary agents that do not enable the extension.

### Design → Seven tools, and the count is the design (Add)

> `build_toolset` still returns exactly seven tools. An embedding host may register extra
> tools explicitly; these count beside the stock tools and the five internal goal tools in
> that host's requests. `read_video` never enters the stock array, stock name-list assertion
> or goal dispatch. Duplicate names are refused by the registry. Only name, description and
> parameters of the extension reach the model.

### Features → The toolset (Add)

> Installed hosts offer `read_video(file_path, mode, start_ms, end_ms, max_frames,
question)`. Every route samples images using FFmpeg. Default `auto` returns images to a
> verified vision model or a text interpretation from the configured analysis model.
> Explicit `frames` and `analyze` select the result form. Support covers every configured
> model the harness supports, including text-only models, through the declared analysis
> route. Native-video input is not required. Source audio is omitted from this image-sampling
> tool, and all results identify the sampled interval and its coverage limitations.

#### Request contract

- `file_path`: required nonempty workspace-relative path, at most 4096 characters. Reject
  absolute paths, URLs, devices, directories and non-regular files. Rooting/symlink checks
  belong to the rooted media-source adapter. Do not use a file-extension allowlist.
- `mode`: `auto` by default. Use `frames` when the active model has verified image input;
  otherwise use `analyze`. Explicit `frames` requires verified image input and explicitly
  fails when unavailable; `analyze` always returns text. The ordinary `auto` route must
  work for every supported configured model with a ready extension, including Unknown
  and Unsupported main-model image capabilities.
- `start_ms`: nonnegative integer, default 0, on the displayed source timeline.
- `end_ms`: exclusive end; if absent, the lesser of source duration and start + 60000.
  Reject empty/reversed/out-of-duration or >60000 ms windows; report the resolved interval.
- `max_frames`: 1–4, default 4, applies to every mode and to analysis input as well as
  direct image output. No analysis path scans/uploads the whole movie instead.
- `question`: optional nonempty text, at most 4096 characters. In analysis, the default asks
  for visible states, actions and errors with the sampled timestamps and omission limits.
  In direct image mode, include it as untrusted task text beside the frame labels.

Examples (the executor applies defaults):

```json
{ "file_path": "recordings/repro.webm", "start_ms": 12000, "end_ms": 20000, "max_frames": 4 }
```

```json
{
  "file_path": "recordings/repro-av1.mp4",
  "mode": "analyze",
  "start_ms": 12000,
  "end_ms": 20000,
  "max_frames": 4,
  "question": "Describe the visible steps and error."
}
```

#### Data flow and result

```text
validated batch → existing ToolPolicy / approval gate → projected-request admission
  → route/capability/analysis-budget preflight → rooted bounded snapshot
  → FFprobe → selected stream/interval → FFmpeg → labelled JPEGs
      frames  → Text manifest + (Text source timestamp, Image JPEG) pairs
      analyze → same JPEGs/labels → same-provider vision model → Text manifest + Text answer
  → existing tool-result log → save → existing answer / Done acknowledgement
```

`VideoReadArguments` is the tool request. `VideoReadManifest` records the requested mode,
resolved mode, source path/SHA-256, detected container and video codec, selected stream index,
source duration/interval, FFmpeg build identity and sampler policy version. Its method is
always `sampled_frames`, audio is always `omitted`, and it includes ordered `VideoFrame`
metadata and warnings in both output forms. The original video remains local.

Each `VideoFrame` has the actual source presentation timestamp, JPEG dimensions and digest.
Sample centres of equally sized portions of the interval; select the nearest displayed
frames within it, de-duplicate timestamps and order them by time. Apply display rotation,
sample aspect ratio and a documented colour conversion to JPEG. Variable frame rate uses
presentation timestamps, not nominal FPS. Normalize nonzero source start times and record
the stream/normalization. Bound FFprobe's frame metadata read; do not enumerate every frame
of a one-hour source in memory. Warn about reduced counts and sampling gaps.

In `frames`, pair each metadata entry with one returned image block in the same order.
In `analyze`, send exactly those images and source-time labels to a tools-disabled vision
request; return no image blocks to the main model. The answer must cite sampled source
timestamps and distinguish observations from inference; it cannot establish events between
samples or anything heard. Source timestamps and frame digests remain in the manifest even
when only the text answer is retained.

`VideoAnalysisProvenance`, present for analysis, records provider, plan, exact model,
allowlisted endpoint origin, protocol, profile version, processing settings and reported
usage. Absent usage stays absent. No credential, query string or remote upload handle enters
it. Record the resolved delivery model rather than presenting an analysis-model answer as
if the main conversation model directly saw the movie.

Put the bounded JSON manifest in the first **Text** block and in `ToolOutcome::Success.value`:
value alone is not the durable transcript. Follow with labels/images or the text answer.
Failures use existing `ToolOutcome::Failure` and the original call ID. No raw-video block,
new link frame, TUI playback, provider file upload or recursive agent invocation is needed.

#### FFmpeg formats and codecs

Use FFprobe for content/stream discovery and FFmpeg for decoding, sampling, scaling and JPEG
encoding, with structured process arguments. “Standard FFmpeg codecs” means the video
formats/decoders enabled in the configured build, not every optional library in every build.
Inspect and report the build's `-version`, `-buildconf`, `-demuxers` and `-decoders` capability
inventory during extension preparation. Container and codec are independent: WebM is a
container; AV1 is a video codec that must also decode in supported MP4/Matroska inputs.

Accept self-contained local video formats that the FFmpeg build can demux and decode under
the existing bounds, including MP4/MOV, Matroska/WebM, AVI and MPEG containers. Do not restrict
inputs to MP4/MOV or invent a provider format restriction on local source files: providers
receive JPEGs. Mandatory release coverage includes H.264/AVC, HEVC/H.265, MPEG-4 Part 2,
MPEG-2, MJPEG, VP8, VP9 and AV1; require WebM/Matroska demuxing and software AV1 decoding.
Optional additional decoders present in FFmpeg remain available under the same safety policy.
Missing a mandatory decoder/demuxer produces an actionable installation/configuration error
before enabling the extension; do not advertise full codec support with a reduced build.

Pin/test the supported FFmpeg/FFprobe build family and require a compatible pair. Discover
its actual components rather than assuming `libdav1d` exists; another working AV1 decoder
can satisfy the requirement. Run codec smoke fixtures when certifying a build. Skip audio,
subtitle and data streams during sampling; choose the default usable video stream, then the
first usable non-attached-picture video stream, and record its index. Refuse no-video inputs,
unsafe playlists/external references or a decoder absent from the build with a precise reason.
Raw streams without sufficient timing for the contract are refused with that reason.

Primary references: [FFmpeg CLI/build discovery](https://ffmpeg.org/ffmpeg.html),
[FFprobe stream inspection](https://ffmpeg.org/ffprobe.html),
[formats/demuxers](https://ffmpeg.org/ffmpeg-formats.html), and
[codec/AV1 decoding](https://ffmpeg.org/ffmpeg-codecs.html). Build-dependent coverage must be
proved with fixtures; codec documentation alone is not local execution evidence.

#### Bounds and capabilities

| Bound                            | Proposed initial policy                                                                                               |
| -------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| Input snapshot / source duration | 128 MiB / one hour; refuse larger before decoding.                                                                    |
| Formats/codecs                   | Self-contained formats enabled in the certified FFmpeg build; WebM and AV1 mandatory.                                 |
| Decoder                          | 2 worker threads, 512 MiB worker memory, 30-second probe/decode deadline.                                             |
| Temporary storage                | 256 MiB per call; one media job per extension instance.                                                               |
| Inspection / complete call       | 60 seconds of source time / 180-second wall deadline.                                                                 |
| Frame output                     | Up to 4 JPEGs, 512 KiB each, both edges ≤1024 pixels, preserving aspect ratio.                                        |
| Analysis media                   | Same ≤4 JPEGs/2 MiB decoded file bytes; existing image profile/request byte caps apply.                               |
| Analysis work                    | One backend request, no automatic retries; target 2048 answer tokens, not a universal generation ceiling.             |
| Provider generation              | Enforce 2048 tokens only where the exact wire supports that ceiling; subscription uses its qualified backend ceiling. |
| Analysis context                 | At most 32000 estimated input tokens, plus the actual output/reasoning reservation described below.                   |
| Manifest + answer + labels       | At most 32 KiB UTF-8 combined; bounded stream collection, overflow cancels and fails.                                 |

These are extension policies, not provider maxima or claims of measured performance.
An adapter must enforce each bound while reading/writing/allocating; checking metadata and
then calling an unbounded `FsPort::read_bytes` is insufficient for large/changing sources.
Snapshot and hash once, then probe/decode that exact snapshot. Probe dimension limits
before allocating; bound source dimensions to 8192 per edge and 33554432 pixels. Container,
codec, dimensions or timestamps that cannot be interpreted safely produce a failure.
A backend profile or declared host envelope may impose smaller limits. The decoder enforces
that smaller envelope; reject a clip that cannot meet it.

#### Aggregate result admission

Direct-frame installation requires a host-owned admission ledger and a generic, opt-in
runner hook. Existing `ToolExecutor::execute(ToolCall)` and `ToolPolicy::decide` cannot read
the session or pending batch, and current per-image result validation does not admit the
next assembled request. The extension must not infer aggregate capacity from these APIs.

The proposed `ToolAdmission` contract has these lifecycle operations:

1. **Project:** after recording calls and resolving ordinary approval, but before dispatching
   any permitted work, the runner supplies an immutable projection of the selected model,
   plan/endpoint/protocol, selection epoch, system prompt, schemas, retained transcript,
   all batch calls and existing output/reasoning reservations. It exposes no mutable Session
   or credential. The host declares a conservative result envelope for every offered tool:
   image count, encoded image/wire bytes, dimensions/profile token charge, serialized text
   and framing overhead, including a bounded failure result. Unknown envelopes make a
   direct-frame host unready; `read_image` and custom image producers participate too.
2. **Reserve:** first prove that the existing transcript plus every call and its bounded
   failure result fits. If this base cannot fit, stop the batch with the existing context
   failure before source/process/analysis I/O. Then atomically reserve success envelopes in
   call order against the base, retained history and all pending reservations, across every
   execution chunk. Use the selected adapter's conservative request estimator and the same
   retained-image validation/fitting rules as `build_request`; never elide the newest turn.
   For video, reserve requested max_frames at worst-case JPEG/base64/wire size, dimensions
   and 32 KiB text, even when actual decoding might be smaller. Reserve analysis text too.
   A call that cannot reserve receives a bounded model-visible failure using its existing
   call ID; its executor is not invoked. No implicit switch to analysis or fewer frames.
3. **Validate/commit:** the runner validates each actual outcome against its envelope and
   the immutable projection before reporting success. An oversized or stale outcome becomes
   the reserved failure; it never enters the log as success. Keep every pending reservation
   until all results have been appended in call order, including denied/interrupted results.
   Reconcile estimates to actual content and check the combined next request before releasing
   the batch ledger. No gap may allow another producer to reuse uncommitted image capacity.
4. **Release:** denied, failed, timed-out or cancelled work releases its success allowance
   only while retaining its failure-result allowance until append. Cancellation/shutdown
   drains or revokes all handles; on resume, reconstruct capacity from durable content.
   Selection changes are queued until results are committed and the next request is built
   under the same epoch. A stale epoch before effects denies dispatch; never recompute
   admission against a different selection after disclosure.

The host supplies owned reservations to the static executors through caller-owned handles
keyed by runner/turn/call ID. No session borrow crosses an await. The runner hook sees all
registered and goal-tool outcomes; its default absence preserves stock behavior. A video
host must not bypass it by wrapping only read_video. Exact-call policy still governs effects;
admission grants capacity, not permission. A host without this integration may expose
analysis-only delivery with ordinary next-step text fitting, but cannot advertise this
proposal's direct-frame preflight guarantee or complete auto configuration.

Counterexample gate: use three four-frame calls with a host-enforced 128 KiB JPEG envelope
and sufficient text/context/byte capacity, isolating the eight-image cap. Reserve the first
two; the third returns a failure before opening its source. With default 512 KiB envelopes,
the encoded-byte cap can refuse earlier; that is correct admission, not a promise to always
fit eight images. Mixed read_image/video calls and prior retained images use the same ledger.
Context exhaustion, byte caps, cancellation and selection races require both directions.

#### Compatibility with every configured model

The extension receives the active selection and exact image capabilities from the host.
`auto` returns pixels only when the main model's provider/plan/endpoint/protocol profile is
Supported. Unknown or Unsupported main-model image input selects sampled-frame analysis,
not a refusal. This does not promote that main model's image capability. Freeze selection
through tool execution and the next request. A stale projection denies dispatch before
any effects; rebuild it before admitting new work, rather than changing an admitted route. Recheck retained image history before a later model switch or resume;
never resend images to an unqualified model. For transcripts intended to move to a text-only
model, use `analyze` so video reads remain text-only. Existing image-history validation
is preserved; all-model tool support does not override an incompatible earlier transcript.

Use a tools-disabled image request to the resolved analysis model. The route keeps the
current provider, endpoint/plan and credential account; a different model does not mean a
different provider or silently reused credential. Extension configuration declares these
routes and budgets when enabled. Hosts may override the analysis model with another
verified model on that same provider/plan. No model-facing argument selects a host, plan,
credential or analysis model. No extra provider account is required by the default routes.

| Current selection                                                                                                                          | Default analysis model on that provider/plan | Qualification needed                                                                          |
| ------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------- | --------------------------------------------------------------------------------------------- |
| DeepSeek API: `deepseek-flash`, `deepseek-v4-pro`                                                                                          | `deepseek-flash`                             | Add/capture/live-verify Flash image profile on the DeepSeek API. Pro stays text-only.         |
| z.ai API or coding: `glm-5.3-flashx`, `glm-5.3-flash`, `glm-5.3`, `glm-5.2`                                                                | `glm-5.3-flash`                              | Qualify image input independently on API and coding endpoints with their respective account.  |
| Anthropic API: `claude-sonnet-5-5`, `claude-opus-5-5`, `claude-fable-5-1`, `claude-haiku-4-5-20251001`, `claude-sonnet-5`, `claude-opus-5` | `claude-sonnet-5-5`                          | Existing exact image profile; verify this tool's sampled-frame request and text replay.       |
| OpenAI API or subscription: `gpt-6-astra`, `gpt-6.1-sol`, `gpt-6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`                     | `gpt-6-luna`                                 | Existing exact image profiles; independently exercise API/subscription streaming and budgets. |

This table is the current inventory, not a new model allowlist. Derive acceptance coverage
from the harness's provider/model/plan catalogue so a new offered model cannot silently
miss a route. Qualify every combination the harness actually supports; a pre-existing
provider-plan catalogue discrepancy is not proof the endpoint accepts the main model.
Unsupported custom endpoints, missing credentials and missing verified analysis routes are
configuration errors, not evidence that a text-only model is incompatible with the tool.

Prepare and validate routes for all supported provider/plan selections before advertising
the extension's full-coverage configuration; at provider switches, preflight the selected
route before another call. If a required route cannot be qualified, full model support is
**not complete**: an eight-model-only release does not satisfy this proposal. No extension
is enabled by default and no provider capability is fabricated to meet that gate.

Existing request-wide caps still apply: at most eight images, 4 MiB request/record,
whole-turn context fitting and output/reasoning reservation. Four images per call do not
guarantee space for four more retained images. Unknown **analysis** capability refuses before
media I/O/HTTP; Unknown **main-model** capability goes to a ready analysis route. Budget/byte
failure in a selected route produces an explicit failure, not an unbudgeted retry or route
change. Audio processing and native movie-input wires are outside this sampling contract.

### Sessions → What a session is (Add)

> A video read persists selected JPEGs and its text manifest, or its backend answer and
> provenance, in existing version-2 tool-result content. Frame metadata and source timing
> accompany either output form. Version-1 reading and the body
> version remain unchanged. A replay does not decode the original movie, repeat a paid
> analysis, re-upload media or require a live provider file handle. A later inspection is
> a new tool call and records a new source digest, even when the path is unchanged.
>
> Frame timestamps survive reload because they are text blocks next to the image blocks.
> Backend answers are labelled model interpretations, with sampling/audio limitations;
> they are not a lossless copy of the video. More detailed follow-up needs another explicit
> read. The source movie is not stored inline in the session.

### Safety → Defaults, and what they do not protect you from (Add)

> The initial external-decoder implementation declares `ToolAccess::Execute`. It runs
> programs and writes extension-owned temporary files; labelling it Read would evade the
> default approval gate. The existing exact-call ToolPolicy can additionally constrain the
> path, interval and installed backend. No policy grant allows model-selected hosts,
> executables, credentials, shell fragments, FFmpeg flags or upload destinations.
>
> Enabling the extension explicitly authorizes its declared same-provider analysis-model
> route and budget. Its credentials stay behind SecretPort/caller-owned secret handles,
> scoped to that provider and plan. OpenAI subscription grants and z.ai coding keys are
> never reused against public API analysis endpoints.

Run a fixed, host-configured decoder binary with structured arguments, sanitized environment
and no shell interpolation. Deny playlists, external media references and remote protocols;
only the authorized snapshot is input. A protocol whitelist alone does not confine local
file access: the adapter must reject external references and prove confinement or use a
host-confined media worker. Unsupported containment/bounds must disable that adapter rather
than claim safety. Nanus maintainers own the adapter, certified build requirements, codec fixtures,
version support and installation guidance in this repository. Hosts/operators supply the
configured FFmpeg/FFprobe executables and any OS worker containment. The extension reports
missing dependencies; it never installs executables implicitly. Ordinary agents do not
require FFmpeg. The
[FFmpeg CLI reference](https://ffmpeg.org/ffmpeg.html) defines media options, not containment.

Cancellation must terminate/reap FFprobe/FFmpeg descendants, cancel an in-flight analysis
request and release/reserve budgets appropriately. Drop guards synchronously revoke local
work and arrange supervised host cleanup. Remove temporary data on success, failure,
timeout and unload. There are no provider movie-file uploads or provider file handles to
clean up; a cancelled image-analysis request may still be billable.

Analysis usage is charged to an explicit host-owned budget before dispatch and reconciled
with reported usage afterwards. Main-loop usage/goal budgets currently measure the main
model; do not pretend they include nested analysis. `VideoAnalyzer` must report an exact
provider/plan/endpoint generation policy separately from its image profile: whether a wire
ceiling is enforced, the qualified total-generation ceiling and whether reasoning shares
that ceiling or has a separate bound. Request estimation alone does not prove enforcement.

For a wire with a verified ceiling, request/reserve 2048 generation tokens plus separately
bounded reasoning only when the provider accounts for it outside that ceiling. For OpenAI
subscription, preserve the encoder's omission of `max_output_tokens`; the local answer target
and `ChatRequest.max_tokens` do not constrain its wire. Reserve the entire qualified backend
generation ceiling instead. The current adapter advertises 128000 tokens; qualification must
establish that this ceiling bounds total generated answer/reasoning for that exact endpoint.
Set the estimate-only `ChatRequest.max_tokens` to that full ceiling, not 2048, so context and
budget fitting reserve it. Do not double-count reasoning covered by a shared ceiling. If
reasoning is separately billed/bounded, reserve its qualified maximum too. Unknown ceilings,
accounting or insufficient full-ceiling budget refuse before source/HTTP; all-model support
requires this endpoint-specific qualification, not an invented subscription wire parameter.

Collect answer/reasoning/events with bounded memory and the complete-call deadline. At the
remaining 32 KiB answer/text allowance or deadline, cancel the request and return a bounded
failure rather than retaining a truncated answer as complete. Local stream cancellation does
not cap remote generation or cost. Keep the full unused reservation for an incomplete or
cancelled charge; reconcile only from trustworthy final usage, or settle the full conservative
charge according to the host ledger policy. Missing usage must never release it as zero.
The host ledger survives turn cancellation/session shutdown; failure accounting cannot rely
on a success manifest. On success, provenance processing records ceiling policy, reservation,
local limits and completion, while usage remains provider-reported or absent. Media text,
subtitles and backend answers are untrusted tool data, never system instructions.

### Testing → The tests that matter most (Add)

> Extension tests drive an installed tool through the ordinary runner and assert actual
> returned pixels, timing labels and persisted content. They keep the seven-tool stock
> assertion, five goal tools, schema allowlist, fail-closed policy and save-before-Done
> behavior intact. Certified FFmpeg builds and every supported main-model route have
> independent codec, captured-image-wire, replay and live sampled-temporal evidence.
> Returning a text error on a text-only model does not count as full model support.

## Type changes

Proposed extension-only schema fragment (Draft 2020-12). Rust domain/port types remain the
implemented authority. This adds no core content variant or global schema sidecar.

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$defs": {
    "VideoReadArguments": {
      "type": "object",
      "additionalProperties": false,
      "required": ["file_path"],
      "properties": {
        "file_path": { "type": "string", "minLength": 1, "maxLength": 4096 },
        "mode": { "enum": ["auto", "frames", "analyze"], "default": "auto" },
        "start_ms": { "type": "integer", "minimum": 0, "maximum": 3600000, "default": 0 },
        "end_ms": { "type": "integer", "minimum": 1, "maximum": 3600000 },
        "max_frames": { "type": "integer", "minimum": 1, "maximum": 4, "default": 4 },
        "question": { "type": "string", "minLength": 1, "maxLength": 4096 }
      }
    },
    "VideoFrame": {
      "type": "object",
      "additionalProperties": false,
      "required": ["timestamp_ms", "width", "height", "sha256"],
      "properties": {
        "timestamp_ms": { "type": "integer", "minimum": 0, "maximum": 3600000 },
        "width": { "type": "integer", "minimum": 1, "maximum": 1024 },
        "height": { "type": "integer", "minimum": 1, "maximum": 1024 },
        "sha256": { "type": "string", "pattern": "^[a-f0-9]{64}$" }
      }
    },
    "VideoAnalysisProvenance": {
      "type": "object",
      "additionalProperties": false,
      "required": [
        "provider",
        "plan",
        "model",
        "endpoint_origin",
        "protocol",
        "profile_version",
        "processing"
      ],
      "properties": {
        "provider": { "type": "string", "minLength": 1, "maxLength": 64 },
        "plan": { "type": "string", "minLength": 1, "maxLength": 64 },
        "model": { "type": "string", "minLength": 1, "maxLength": 256 },
        "endpoint_origin": { "type": "string", "format": "uri", "maxLength": 256 },
        "protocol": { "type": "string", "minLength": 1, "maxLength": 64 },
        "profile_version": { "type": "string", "minLength": 1, "maxLength": 128 },
        "processing": { "type": "string", "minLength": 1, "maxLength": 1024 },
        "usage": {
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "input_tokens": { "type": "integer", "minimum": 0 },
            "output_tokens": { "type": "integer", "minimum": 0 },
            "reasoning_tokens": { "type": "integer", "minimum": 0 }
          },
          "minProperties": 1
        }
      }
    },
    "VideoReadManifest": {
      "type": "object",
      "additionalProperties": false,
      "required": [
        "file_path",
        "source_sha256",
        "duration_ms",
        "start_ms",
        "end_ms",
        "mode",
        "method",
        "audio",
        "frames",
        "warnings",
        "requested_mode",
        "container",
        "video_codec",
        "video_stream",
        "decoder_version",
        "sampler_version"
      ],
      "properties": {
        "file_path": { "type": "string", "minLength": 1, "maxLength": 4096 },
        "source_sha256": { "type": "string", "pattern": "^[a-f0-9]{64}$" },
        "duration_ms": { "type": "integer", "minimum": 1, "maximum": 3600000 },
        "start_ms": { "type": "integer", "minimum": 0, "maximum": 3600000 },
        "end_ms": { "type": "integer", "minimum": 1, "maximum": 3600000 },
        "mode": { "enum": ["frames", "analyze"] },
        "method": { "const": "sampled_frames" },
        "audio": { "const": "omitted" },
        "frames": {
          "type": "array",
          "maxItems": 4,
          "items": { "$ref": "#/$defs/VideoFrame" },
          "minItems": 1
        },
        "warnings": {
          "type": "array",
          "maxItems": 16,
          "items": { "type": "string", "maxLength": 1024 }
        },
        "backend": { "$ref": "#/$defs/VideoAnalysisProvenance" },
        "requested_mode": { "enum": ["auto", "frames", "analyze"] },
        "container": { "type": "string", "minLength": 1, "maxLength": 256 },
        "video_codec": { "type": "string", "minLength": 1, "maxLength": 256 },
        "decoder_version": { "type": "string", "minLength": 1, "maxLength": 256 },
        "sampler_version": { "type": "string", "minLength": 1, "maxLength": 256 },
        "video_stream": { "type": "integer", "minimum": 0, "maximum": 1023 }
      },
      "allOf": [
        {
          "if": { "properties": { "mode": { "const": "frames" } } },
          "then": { "not": { "required": ["backend"] } },
          "else": { "required": ["backend"] }
        }
      ]
    }
  }
}
```

Runtime validation additionally enforces rooted paths, whitespace-only strings, start < end
≤duration, ≤60-second interval, ordered in-window timestamps, metadata/image digest pairing,
profile compatibility and total serialized bytes. These cross-field/content rules are not
expressible in ordinary JSON Schema alone.

## Implementation notes

Baseline: parent `4febd453`; clean original working copy, new JJ workspace `read-video`,
bookmark `codex/read-video-design`. CodeGraph located the boundaries; current on-disk source
confirmed the registry has registration but **no tool removal API**. Suggested public factory:
`read_video_tool(services, policy) -> Result<ToolDefinition, VideoError>` in the repo-owned
optional `crates/nanus-tool-video` workspace crate, never `build_toolset`.

The extension-local service boundary has three dyn-compatible, local-future interfaces:
a rooted `VideoSource` creates a bounded snapshot; `FfmpegDecoder` implements `VideoDecoder`
with FFprobe/FFmpeg; a `VideoAnalyzer` reports its exact image and generation profiles and
interprets labelled sampled JPEGs. The crate owns these contracts and a route resolver for
every supported model. A trusted host constructs all handles, supplies active capability
resolution, the shared admission ledger and analysis-charge reservations, and translates
one extension error enum into model-visible failures. Keep media/analysis interfaces local.
The generic `ToolAdmission` boundary belongs in `nanus-ports`, with immutable in-memory
projection/envelope/reservation contracts and opt-in callbacks in `AgentRunner`; these are
new proposed APIs, not schema/wire entities or existing capabilities. The runner supplies
batch/session context; the host owns capacity and charge state. Default/minimal composition
must keep working without an admission implementation or dependency on the video crate.

| Existing entry point                                                                             | Reuse / implication                                                                                                             |
| ------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------- |
| `crates/nanus-domain/src/tool.rs` — ToolDefinition, ToolExecutor, ToolRegistry                   | Existing schema/access/executor seam; explicit registration with duplicate refusal.                                             |
| `crates/nanus-bundle/src/lib.rs` — build_toolset, ToolRegistryHandle, tools_plugin               | Keep stock set intact; host creates one shared registry before runner startup.                                                  |
| `crates/nanus-bundle/src/agent_loop.rs` — gate, run_tools, validate_result_images, build_request | Reuse policy/cancellation; add optional generic admission projection/reservation/commit callbacks, without read_video dispatch. |
| `crates/nanus-ports/src/control.rs` — ToolPolicy, TurnControl                                    | Add separate ToolAdmission contracts; policy stays an exact-call permission decision.                                           |
| `crates/nanus-adapter-openai/src/responses.rs`; capabilities estimate                            | Preserve subscription ceiling omission; qualify generation accounting and reserve the actual ceiling in local estimation.       |
| `crates/nanus-bundle/src/tools/read.rs` — read_image_tool                                        | Pattern for typed image results, not a suitable whole-video reader.                                                             |
| `crates/nanus-domain/src/content.rs`; `nanus-ports/src/capabilities.rs`                          | Reuse media validation/limits and exact-model request estimation.                                                               |
| `crates/nanus-ports/src/fs.rs` — FsPort; `control.rs` — TurnControl                              | Existing rooting/cancellation concepts; bounded snapshot I/O belongs to host media adapter.                                     |
| `crates/nanus-domain/src/session.rs`; adapter-store; examples/embedded                           | Existing v2 persistence and host-provided tools; add installation/replay examples when shipped.                                 |

Implementation starts with generic batch admission and endpoint generation-budget policies,
then the repo-owned crate and FFmpeg/FFprobe probing/sampling, `auto` routing and tools-disabled
sampled-frame analysis on existing adapter wires. Add
DeepSeek Flash and z.ai Flash image profiles as required provider prerequisites with captured
and live evidence; keep text-only main models unpromoted. Exercise Anthropic/OpenAI defaults
on both applicable OpenAI endpoints. Do not declare success after only the eight currently
verified main models work. No native-video wire, external MCP runtime or new provider is
required for this extension.

## Acceptance criteria

1. `crates/nanus-tool-video` is in this repo/workspace, with adapter, docs and fixtures.
   Its opt-in installation adds one ordinary tool; stock seven/five assertions and minimal
   bundle/downstream builds remain unchanged. Registry count, schemas and dispatch agree.
2. Real FFmpeg/FFprobe fixtures return distinct JPEGs with correct source PTS/order,
   rotation/aspect ratio and source offsets, including variable FPS, short clips, duplicate
   PTS and rapid events between samples that must not be claimed as observed.
3. Cover every mandatory decoder and standard container family, including WebM with VP8,
   VP9 and AV1, AV1 in MP4/Matroska and the listed common codecs. A format outside the old
   MP4/MOV pair works when the build supports it. Missing AV1/WebM support fails preparation
   with actionable errors; corrupt media, unsupported optional codecs and external references
   fail safely. Test both positive and negative source/argument/resource bounds.
4. Parameterize from every currently offered/supported main-model/provider/plan combination.
   `auto` yields real pixels for verified main vision models and a meaningful text result
   for text-only/Unknown main models. No provider is excluded and no main model is changed.
   Capture that unqualified/text-only main-model requests contain no image blocks and that
   the analysis request uses the declared same-provider model/endpoint/credential account.
5. Policy denial, missing required analysis profile, configuration/budget failure or unsafe
   FFmpeg build prevents source I/O/process/HTTP. A main-model Unknown image capability alone
   does not deny `auto`. Explicit `frames` on it fails. Selection changes cannot disclose
   images against stale capabilities. The generic admission integration covers retained history,
   every pending chunk and all image producers. Test three four-frame calls, mixed read_image
   calls and serialized-byte/context exhaustion: refuse over-capacity calls before their
   source I/O, preserve bounded failure slots and append only admitted results. Test concurrent
   reservations, cancellation/release, oversized actual outcomes and selection epochs.
   If even the batch's failure projection cannot fit, stop before any permitted work.
6. Captured analysis input contains exactly the selected JPEG bytes, source-time labels,
   question and bounded request settings, no tools, credentials or raw movie/audio. One
   request/no retries and reservation reconciliation are enforced. Capture subscription
   requests with no max_output_tokens and a local full-ceiling estimate/reservation, including
   shared/separate reasoning accounting. Simulate generation beyond 2048, output overflow,
   missing final usage and cancellation: cancel/bound local retention, return failure and
   retain/settle the conservative charge without under-reserving. Unknown generation bounds
   and insufficient budgets refuse before media I/O/HTTP. Qualify the actual endpoint ceiling.
   Source codec never becomes a provider media-format requirement. Answers identify omissions.
7. Native platform lifecycle tests prove FFprobe/FFmpeg child/grandchild teardown and temp
   cleanup for success, failure, timeout, cancellation and shutdown. Codec/build certification
   and resource/containment evidence are required on supported macOS/Linux/Windows targets.
8. Both output forms round-trip version-2 logs and support a follow-up after the source is
   deleted; frame metadata/provenance/timestamps/IDs survive. Version-1 fixtures, bounded
   atomic saves, text-only projections and save-before-Done remain correct. Analysis replay
   never reissues work or tries to send pixels to the text-only main model.
9. Qualify each analysis default per endpoint/plan with real sampled-image bytes, a live
   temporal answer and persisted follow-up. Separately exercise `auto` through every current
   executable main-model selection; a successful vision helper alone does not prove routing
   coverage. Record exact IDs, profile/build/sampler versions, fixture/request digests and
   retrieval date. No live/codec experiments were run for this design update.
10. Run all AGENTS.md gates plus extension/downstream feature matrices. The implementation
    remains incomplete until full model routing, mandatory codecs and platform gates pass.
    Update canonical docs with demonstrated behavior only; this draft remains Proposed.

## Merge plan

1. Accept the design and implement the optional package/host example without changing
   stock composition. Certify FFmpeg builds/codecs and qualify every configured model route.
2. Apply the six Add blocks to their named canonical headings after acceptance tests;
   index any new extension usage page in docs/README.md. Keep provider research dated.
3. Implement the fragment as extension argument/manifest validation. Keep core session
   format and content types unchanged; any later raw-video format needs its own proposal.
4. Once implemented and verified, mark Merged, date and move this file to changes/merged/;
   repair relative links and the .specs index. The research note remains dated evidence.

## Assumptions and open questions

**Assumptions**

- A host installs trusted executors; dynamic untrusted code loading is not required.
- Every supported provider/plan can supply the declared vision-analysis route using its
  own credential account; that route needs qualification and an explicit budget.
- Provider support is endpoint/model/plan-specific and may change after the research date.

**Decisions**

- _Provider scope._ **Existing harness providers only.** Re-evaluate additional providers
  after their harness integration ships, as requested by the owner on 1 October 2026.
- _Maintenance._ **`crates/nanus-tool-video` in this repository.** The owner requested
  repo ownership; Nanus maintains the implementation, FFmpeg integration, docs and tests.
- _Delivery._ **Optional ToolDefinition crate with explicit installation.** Keeps it out
  of the stock seven tools while allowing all configured main models to call it.
- _Decoder._ **FFprobe plus FFmpeg sampling for every route.** Accept the configured build's
  standard formats/codecs and require WebM and AV1, as requested by the owner.
- _Compatibility._ **Default auto routing and same-provider vision analysis.** Text-only
  models receive grounded text; verified vision models receive images without a model change.
- _Lifecycle._ **Installation fixed for a runner lifetime.** Avoids promising reversibility
  that ToolRegistry cannot currently provide; a live removal mechanism is a separate change.
- _Modes._ **Auto, frames or sampled-frame analysis.** One decoder pipeline; no native
  movie input or provider-specific source codec requirement.
- _Persistence._ **Existing text/image v2 blocks.** Keeps durable follow-up without large
  movie blobs, expired remote references or a format migration.
- _Sampling._ **Four frames across an explicit bounded interval.** Fits existing result caps;
  temporal coverage and provider limits remain visible rather than inferred from a model name.
- _Admission._ **Generic host/runner batch reservations.** Static executors cannot project
  retained history; reserve all tool outcomes before dispatch and commit them in call order.
- _Generation budgets._ **Endpoint-enforced ceiling or full qualified backend reservation.**
  Subscription omits its unsupported wire limit; local answer truncation cannot bound billing.
- _Access._ **Execute for external decoding.** Faithfully describes the process capability.
- _Audio._ **Omitted from image sampling.** No speech or acoustic evidence is claimed;
  audio transcription requires a separate contract.

**Open questions**

- Which certified FFmpeg/FFprobe versions/distributions will installation guidance use
  on macOS, Linux and Windows? Codec availability, licensing and worker bounds need evidence.
- Qualify the required DeepSeek Flash and z.ai Flash image routes on their exact endpoints;
  no full-coverage claim is valid until those provider prerequisites pass.
- Verify subscription total-generation/reasoning accounting at the qualified backend ceiling
  and the host's conservative unresolved-charge settlement; advertised model caps alone are
  insufficient evidence for a billed-work bound.
- Is a future removable tool-contribution service needed? It requires ownership tokens,
  reserved-name checks, schema snapshots and draining outstanding calls on unload.
