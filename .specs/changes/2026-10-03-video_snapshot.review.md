# Caller-owned video snapshots — semi-formal certificate

**Date:** 2026-10-03 · **Scope:** Local Nanus seam; not published or adopted by Hype

## Premises

P1. Hype must implement the optional extension's VideoSource through its own bounded,
rooted snapshot authority rather than raising the small authoring-file cap or invoking
stock composition. P2. The published 80a revision has a public Snapshot return type
with a private TempDir field and no public constructor; an external direct source
cannot construct it. Its exact published media.rs bytes match the captured baseline.
P3. Host authority, immutable byte/digest/identity verification and managed process
ownership must remain host responsibilities. P4. Stock source behavior, generic ports,
tool count and minimal embedding stay unchanged; Nanus gains no skill/plugin/workflow.

## Function resolution

The actual public Snapshot::from_owned_file checks absolute/fixed source basename,
parent/current components, SHA-256 shape and the existing source ceiling. It calls no
filesystem, process, environment, provider or credential API. It moves a supplied
opaque Arc<dyn Any + Send + Sync> into the private owner field. Two postconditions
assert absolute path and bounded length. It validates receipt shape only, never
authenticates a file or claims a path is in an approved workspace.

retain_owner clones that same Arc. Its final reference drops the concrete host owner,
so an app can retain a TempDir/receipt/cleanup lease through physical process/reader
join. Debug renders only the existing path/digest/length and hides the opaque owner.
Stock FsSource still reads through its FsPort, enforces its 128 MiB ceiling, computes
the digest and writes the fixed source file; it now stores Arc::new(directory) instead
of Option<TempDir>. Stock relative temp-root and empty-file behavior is preserved by
its existing internal construction path; the new host constructor does not rewrite it.

The external fixture implements the real VideoSource trait and the real VideoDecoder/
VideoRouting ports. Public ToolDefinition::execute resolves to ReadVideoExecutor,
which routes before source I/O, calls the supplied source, probes/samples its copy,
drops the logical Snapshot, then delivers manifest/text/typed JPEG through the existing
validated outcome path. The fixture asserts successful frames manifest, actual image
content and final cleanup. No model or stock composition participates.

## Execution traces

Before: caller VideoSource → needs Snapshot → private cleanup field/no public constructor
prevents construction. After: caller verifies bytes/authority → constructor admits bounded
receipt and takes the owner → tool's decoder borrows exactly that copy → delivery retains
actual pixels and source identity. Normal source release removes the copied file.

Snapshot owner plus physical worker clone → logical snapshot drops → file still exists,
cleanup count zero → worker is explicitly released and joined → final Arc release drops
the host TempDir exactly once and the file disappears. Receipt refusal with a second
owner retained releases only the transferred reference; the copy stays until the other
owner releases. Relative/non-fixed/parent path, short/nonhex digest and excess claimed
length refuse. Zero/below/at the ceiling remain representable without reading a source;
a nonexistent fixed path can have a structurally valid trusted-host receipt. This is
explicit evidence that construction does not perform discovery or create authority.

## Regression evidence

Four new external fixtures pass within eleven library/snapshot nextest cases; all ten
existing real FFmpeg integration cases pass, including source cleanup on success/failure
and actual codec/container/pixel ordering. Final workspace all-target/all-feature Clippy
denies warnings; formatting passes. Actual x86_64-pc-windows-msvc library Clippy passes
with pinned Rust 1.98; it is compilation, not Windows execution. The package doctest
command passes with zero examples. Tests use fictional private copies, fake routing/
decoding for new cases, and local FFmpeg for existing cases. No keychain, browser profile,
paid model, live video account or credential-aware stock workspace suite is executed.

Initial external fixture compilation tried a private executor field and a tuple-shaped
Success; public execution and the actual struct outcome correct those assumptions. Initial
Clippy identifies an underscore-prefixed field now used by retain_owner and a redundant
initializer; the final field and public method names are corrected without suppression.
Initial and final logs and guarded owner baselines reside under /private/tmp/hype-video-snapshot.

## Edges and verdict

**CORRECT**, confidence high, for external construction and final-reference lifetime
semantics proven by the public downstream fixture and explicit physical worker join.
The host must still verify rooted authority, regular-file identity, actual bytes/digest,
immutable storage, approvals and publication. Arc retention is not process cancellation,
hard deadline or crash cleanup; the decoder must retain the owner before spawning physical
work and release it only after teardown. Hype integration, result/request/image limits,
analysis approvals/usage, immutable adoption, native execution and original acceptance
remain open. No source cap, tool registry, provider route, release flag or push changes.
