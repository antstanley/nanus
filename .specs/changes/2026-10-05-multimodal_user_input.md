# Direct multimodal user input

Status: implemented in the isolated candidate; all fifteen scoped library gates passed; final downstream adoption verification remains in progress.

## Problem and scope

Video analysis currently fabricates an assistant tool call and tool schema to deliver frames.
The harness needs ordered typed user content independently of tools. This is a generic library
contract; no skills, plugins, app brief, filesystem resolution or provider promotion belongs here.

## Contract

`Message::User` and `SessionEvent::UserMessage` gain optional `content_blocks` using the existing
bounded `ContentBlock` representation. Absence preserves legacy text and serialized message hashes.
Present blocks are authoritative model input, in exact order; text is a display projection only.
`Message::user_with_content` validates before deriving that projection; plain `user` stays unchanged.
Image-only input is not empty. Shared capability admission, prospective cost and actual dispatch
validate user and tool images identically, retaining exact endpoint/model/protocol qualification.
The existing 32-block, four-image per message, 512 KiB per image, request image/profile limits,
4 MiB record/request and 64 MiB session limits apply. No remote URL/path is read or inferred.

Anthropic gets user text/image blocks; Chat adapters get user text/image_url blocks; Responses gets
user input_text/input_image items. There is no invented call, result, schema or attachment label.
Original pixels and ordering survive session save/load, human-turn fitting and replay hashes.
Unsupported profiles, malformed content and over-limit histories fail before HTTP. Raw foreign
content arrays containing unsupported media fail instead of silently discarding nontext parts.

Writers emit session body version 3. Readers accept 1, 2 and 3; versions 1/2 cannot introduce typed
user content (including a null field). Existing version-1 tool/replay restrictions remain. This
prevents an old version-2 reader from accepting a new record while silently dropping its images.
There is no bulk migration. Existing text CLI/link entry points stay text-only. Library hosts can
compose typed ChatRequests and store typed user events. Video analysis uses system + one typed user
message with question, window, timestamp labels and frames, and an empty tool catalogue.

## Acceptance and review

Prove exact ordered pixel equality across each wire and JSONL reload, image-only visibility,
legacy message hash/JSON preservation, v1/v2 compatibility, old-version smuggling refusal, malformed
and oversized input refusal, combined user/tool image counting and paid request refusal before I/O.
Rerun stock video analyzer fixtures, minimal runner, provider/store suites and downstream compilation.
Downstream adoption and live/native acceptance are separate gates; no capability evidence is promoted here.

Semi-formal design review: the former User deserializer discarded content_blocks, shared image
admission only inspected Tool, and a v2 reader ignored unknown UserMessage fields. Fixing only the
encoder would leave reload and admission unsound. The proposed Message -> shared admission -> wire
and SessionEvent -> v3 writer -> version-aware reader -> Message fold paths close those gaps.
Existing human boundaries retain the User variant and plaintext constructors emit no new field,
so source fitting and default legacy replay hashes remain unchanged. Implementation verdict pending
negative tests and source review.


## Implementation review (semi-formal)

Premises: user input must preserve its ordered content through serialization, provider admission,
wire encoding and session reload; plain text must preserve its old JSON. No tool authority may be
manufactured to deliver a video frame. Provider capability evidence stays unchanged.

Function resolution: `Message::user_with_content` calls domain `content::validate_blocks` before
rendering the display summary. `MessageFields::finish` now retains user blocks and refuses them on
system/assistant roles. `Message::content_blocks` supplies both user and tool blocks to ports
`has_images`, `validate_images` and `visual_cost`; actual provider dispatch uses these shared
checks. Anthropic `encode_content`, Chat `encode_user` and Responses `encode_input_block` translate
blocks in order. `Session::try_to_jsonl` validates typed user records, `validate_body_version`
rejects old-header smuggling, and `SessionLog::derive_messages` restores the blocks. The old
text constructors still omit the optional field; source receipts therefore keep old text hashes.

Execution traces: user PNG/JPEG -> typed session -> atomic store reload -> each provider's user
wire retains the original bytes and interleaved labels. A changed image after an original Responses
answer fails preparation, even after a new user turn and changed trusted instructions. Nine retained
images across user/tool messages fail fitted-request admission; original histories may exceed the
request count before whole-turn fitting. Malformed text-only typed lists also fail validation.
A foreign image array cannot be flattened to text. Version-1/2 text still loads; typed user fields,
including null, refuse under those headers. Version-2 tool image records remain readable.

Video trace: question + sampled interval + timestamp/JPEG pairs -> one typed User following System
-> empty tool catalogue. Existing stream handling still refuses any attempted tool call. Budget,
same-provider routing, cancellation and sampling ownership are unchanged. This is library support;
app prompts, dependency installation and skill/plugin discovery are absent from this path.

Findings fixed: User deserialization previously discarded blocks; admission/cost inspected only
Tool; version-2 readers ignored unknown user fields; raw foreign media arrays silently lost images;
text-only typed lists bypassed early validation; the approximate fallback ignored typed user bytes.
All now have explicit handling and negative fixtures. The initial new test referenced a private
projection helper and the public doctest omitted the optional field; both were corrected.

Verdict: LIKELY_CORRECT for the implemented local contract, pending completion of the recorded
regression gates and consumer validation. Wire/reload evidence does not claim a paid provider run,
native Windows runtime, release publication or complete downstream migration acceptance.


Final library verification: fifteen scoped gates passed with 256 frozen runtime/config/test inputs
unchanged. This includes 563 provider/domain/ports/store tests, 27 video tests, 183 minimal-runner
cases, 53 all-feature admission/embedding cases, 29/35 downstream embedding cases, two minimal
runner doctests and 342 runtime-free TUI cases (overlapping suites, not an additive total).
Workspace/minimal/downstream/TUI Clippy, formatting and Windows-target minimal-library Clippy pass.
No credential-aware stock runtime, paid provider or native Windows runtime was exercised.
The source review extracted assistant folding, signed-wire assembly and shared replay validation
without changing those policies. New/reworked behavior functions fit the 70-line limit; six
pre-existing longer callers have only mechanical optional-field changes. Saved evidence is under
`/private/tmp/nanus-multimodal/`, including the earlier successful run before extraction and the
corrected doctest, module-path and redundant-field findings. No test or lint was disabled.

The consumer's direct-input OpenAI native fixture and nine existing native video cases passed before the
behavior-preserving extraction, using selected FFmpeg/ffprobe, disposable media and scripted HTTP.
The consumer must bind this final source and rerun its full verification before claiming adoption.

Isolated source: `36b174bfdaf1224f8f3613a6f972958e14463292` (`codex/direct-user-images`).
This owner-repository document does not imply the owner working tree runtime was overwritten or adopted.
