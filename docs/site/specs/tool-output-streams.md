# Tool Output Streams And Artifact Reads

Status: implemented; correctness and performance/code-quality reviews passed.

## Scope

Make single and named multi-stream output first-class tool capabilities. The
runtime, not individual tools, owns output capture and artifact retention. Separate
model-visible output from UI display. Replace `read_tool_result` with artifact
reads through the existing `read` tool.

This contract supersedes the Bash-specific stream readback and separate
`read_tool_result` contracts. It preserves full-active-history compaction and its
single context-size-triggered pruning retry. No unrelated refactoring or new
authorization model is required.

## Tool Output Contract

A tool declares either single output or named output streams. Named streams are
not limited to Bash's `stdout` and `stderr`; another tool can declare `results`
and `diagnostics`. Declaration order determines model rendering order. Stream
names must be nonempty, unique, bounded, URI-safe single path segments. Reject
path separators, traversal names, control characters, and invalid declarations.
Use one shared validator and documented limits for declarations and URI lookup.

Tools can produce output incrementally or provide it once at completion:

- A delta carries append-only text chunks, keyed by stream name for multi-stream
  output, and an optional `display` string for the UI. A single-output delta has
  one unnamed output channel. Reject chunks for undeclared streams.
- Delta `display` text appends to the live UI display. It is not authoritative
  tool output and must not be inserted into a stream implicitly.
- Completion carries a final `display` string, terminal status, and the existing
  applicable result metadata, attachments, and emitted messages. Final display
  replaces the accumulated live display; it is not appended a second time.
- A streamed completion finalizes accumulated output. It does not resend or
  duplicate the full stream contents.
- A non-streaming completion supplies its full single output or named outputs
  once. The runtime feeds these through the same capture and retention pipeline.
- Make completion modes unambiguous in the types. Reject an attempt to both
  finalize accumulated output and resupply it as full terminal output.

These are semantic requirements, not a prescription to add parallel APIs.
Adapt the existing tool execution, progress, and completion types with the fewest
coherent changes. Preserve existing cancellation and prepared-operation behavior.

## Runtime-Owned Retention

For each output channel the runtime incrementally appends accepted chunks to a
temporary capture file, tracks byte and line counts, computes a streaming digest,
and keeps only a bounded preview in memory. Do not concatenate unbounded output
in tools, event queues, UI state, or runtime memory. Preserve UTF-8 correctly
across capture, preview, and paging boundaries.

Within a stream, accepted chunks retain emission order. There is no required
ordering between different streams. Apply backpressure to output producers;
never silently drop authoritative chunks when a queue fills.

At completion, finalize content-addressed artifacts using the existing artifact
store and atomically publish terminal result references:

- Single output has one full-output artifact, without a synthetic stream heading.
- Named output has one artifact per stream and a generic manifest mapping stream
  names to those artifacts in deterministic declaration order. Preserve declared
  empty streams. The manifest is not Bash-specific.
- Bash emits ordinary named streams through this interface. Remove its private
  capture/manifest/retention implementation where the runtime replaces it.
- Non-streaming tools do not need to know artifact IDs or write artifact files.
- Self-paginating tools with the existing absolute truncation opt-out, especially
  `read`, return their bounded page without another output-retention/truncation
  cycle. Finalization must honor this exception rather than retaining every
  artifact read as a new artifact.

Failure and cancellation finalize output already accepted by the runtime and
mark it incomplete with the correct terminal status. They must not present it
as successful output or lose it solely because the tool did not complete normally.
Keep disk exhaustion, capture errors, size bounds, cleanup, and resource ownership
in the runtime. Reuse existing resource limits; make any newly necessary bounded
limits explicit and tested rather than inventing hidden unlimited buffers.

Persist terminal references before releasing the artifact publication protection.
Garbage collection must follow generic manifest references transitively and retain
their stream artifacts while the owning result is live. Preserve content-addressed
deduplication, integrity checks, existing grace periods, live-capture locks, and
crash cleanup. A replay reconstructs the same final output and display without
rerunning the tool or replaying output chunks into finalized artifacts twice.

## Model Presentation

The model receives authoritative output previews, not `display`. Render named
streams separately:

```text
[stdout]
<stdout preview>
[Truncated. Read more: read(filePath="artifact://<digest>/stdout", offset=...)]

[stderr]
<stderr preview>
[Truncated. Read more: read(filePath="artifact://<digest>/stderr", offset=...)]
```

The digest in named-stream hints identifies the generic manifest. Each stream
uses the configured `[tool_output]` line and byte limits independently. Emit a
read hint only when that stream was truncated. The hint must address the full
original stream and contain a correct zero-based continuation offset. If a byte
limit cuts a line, the hint must allow that line to be recovered without skipping
unseen content. Single output renders one preview without a stream heading and
uses `artifact://<digest>` for its full-output hint.

Preserve the existing aggregate event/model payload safety bounds. Bound stream
declarations and preview construction so multiplying per-stream limits cannot
produce an invalid oversized result or unbounded allocation. If a hard aggregate
bound prevents rendering a preview in full, expose a truthful truncation/read hint
or report the existing resource-limit error; never silently lose output.

## UI Presentation

UI live output is built from delta `display` fields; final output is the terminal
`display` field. Neither is implicitly reconstructed by merging named streams.
Tools choose useful display text, while the runtime continues to own actual output
capture. Preserve existing title, status, timing, expansion, and safe terminal
text handling. Bound display accumulation independently of retained full output.
Replay must show the same final display as live completion, with no duplicate
delta text. Failures and cancellations must have a meaningful incomplete display.

## Artifact Reads Through `read`

Keep the existing `read` argument name `filePath` and offset/limit conventions:

```text
read(filePath="artifact://<64 lowercase hex characters>", offset=0, limit=200)
read(filePath="artifact://<64 lowercase hex characters>/stdout", offset=0, limit=200)
```

Dispatch `artifact://` before filesystem normalization, home expansion, existence
checks, or filesystem permission preparation. Do not treat malformed artifact
URIs as fallback filesystem paths. A bare artifact URI reads stored text as-is;
for a manifest it returns manifest JSON. A stream suffix selects a declared stream
from the generic manifest. Unknown streams and stream selection on non-manifests
produce clear errors. There is no separate `stream` argument on `read`.

Artifact reads use zero-based line offsets, a default and maximum of 2,000 lines,
positive limits, continuation metadata, and the existing 2 MiB result bound.
Oversized pages fail with a resource-limit error rather than silent truncation.
Preserve ordinary filesystem `read` formatting/media/paging behavior; artifact
pages are stored content, without filesystem wrappers or synthetic line numbers.

Artifact IDs grant access by possession within the configured artifact store.
Do not add session-ownership, tool-call visibility, or revert checks to artifact
lookup. Keep normal tool approval/preparation/cancellation mechanisms and do not
weaken filesystem-read permissions. Artifact resources must be represented as
artifact reads, not passed through path permissions accidentally.

Validate the exact URI grammar, lowercase 64-character digest, optional stream
segment, and absence of extra path components, query, fragment, or traversal.
Preserve store-boundary and digest-integrity checks and normal missing/corrupt
artifact errors. Internal `ArtifactReference` URIs can remain
`artifact://sha256/<digest>`; public read syntax does not require storage-format
or saved-reference churn.

Remove `read_tool_result` completely from the active tool system: provider,
registration, argument schema, permission-name registry, descriptions, guidance,
and retrieval hints. Do not retain a compatibility alias or a test-only retired
reader. Necessary historical tool-call identities remain decodable in saved logs.
Tool-call IDs remain for correlation, not artifact retrieval.

## Compaction Rules

Preserve these existing behaviors while recognizing artifact retrieval by the
combination of tool name `read` and its structured `filePath` argument:

1. The first summarizer trial includes the entire active history, including prior
   summary and recent turns retained afterward. Do not resurrect discarded logs.
2. Only an actual summarizer-input local fit rejection or provider context-length
   failure permits one pruned retry. Parent overflow alone and unrelated errors
   do not trigger pruning.
3. In that private retry snapshot, replace ordinary tool output with usable
   artifact read references, regardless of output size or recency.
4. Redact all output from artifact `read` calls instead of retaining another copy:
   content, metadata, attachments, and tool-emitted messages. This applies to all
   streams and both small and recent results. Keep call/result pairing intact.
5. Ordinary filesystem `read` is not artifact retrieval and follows the ordinary
   tool-output pruning path. Parse arguments structurally, not by substring
   matching serialized JSON or matching the tool name alone.
6. Do not create another artifact containing a redacted artifact-read result.
7. Pruning must not mutate saved history or the recent suffix retained after the
   summary. Failure, empty/non-text output, or an invalid summary leaves the
   original context intact. No durable elision events for the private retry.
8. All generated truncation, nested full-output, and compaction retry hints must
   use `read` with the public artifact URI. Native-provider compaction keeps its
   existing behavior except for generic output-reference integration required here.

## Compatibility And Integration

This is an intentional tool execution/event contract change. Prefer a coherent
replacement to compatibility layers for authored tool APIs. Preserve the repo's
versionless best-effort saved-event reading contract and avoid gratuitous changes
to historical data. Update protocol version and generated bindings only where
the real wire-contract change requires them; follow the contributor guide for
all coupled references. Do not claim the wire protocol is unchanged if it changes.

Integrate built-ins, Bash, MCP/plugin adapters, server subscriptions, SDK surfaces,
model history, TUI, replay, artifact GC, tool registration, and docs wherever they
consume affected types. Tools without streaming support use terminal single output
through the same runtime pipeline. Existing attachments and emitted-message role
constraints remain intact. Do not weaken approval, filesystem, or plugin isolation.

## Acceptance And Verification

Implementation is complete only after tests cover:

- Single, multiple, empty, and custom named streams; declaration/order validation.
- Interleaved deltas; authoritative chunks independent of display; streamed
  completion without duplication; non-streaming completion; invalid mixed modes.
- Large output with bounded memory/queues/previews, capture errors, cancellation,
  failure, finalization, replay, and resource cleanup.
- Independent byte/line truncation, exact boundaries, partial lines, UTF-8, correct
  hints, and aggregate payload bounds.
- Generic manifest readback and each named stream via public `read`, including
  content beyond previews, paging, invalid URIs, unknown streams, corruption,
  missing artifacts, and possession-based access across sessions/reverts.
- Full retention lifetime through generic manifest GC, deduplication/publication,
  and active capture cleanup protection.
- No repeated truncation/retention of artifact pages; unchanged filesystem/media
  reads and their permission checks; absence of the removed tool registration.
- UI append/final-replacement semantics, bounded rendering, and live/replay parity.
- Full-history compaction, context-only retry, artifact-read-only redaction,
  unchanged recent suffix/logs, and failure preserving original context.

Run the required build, workspace tests, formatting, stable/MSRV Clippy, generated
bindings, strict docs build, and dependency checks from `AGENTS.md`. Have a reviewer
check the implementation against this spec, fix actionable findings, and re-review.
Update this page's implementation status only after verification succeeds.
